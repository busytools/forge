//! The tool surface: what a session is offered, transcribed from
//! `@playwright/mcp` 0.0.83's own `tools/list` - the capture preserved with
//! the V1 spec.
//!
//! **Names, descriptions and argument schemas are upstream's, exactly.**
//! A prompt written against a Playwright MCP server names `browser_navigate`
//! and passes a snapshot's `target` ref, so a renamed tool or a moved
//! argument is a prompt that resolves to something else. The additions this
//! family carries beyond upstream are marked as such, and each names an
//! argument upstream does not have - `force`, `expression` - or stands as a
//! tool of its own built from the same driver.
//!
//! Kept apart from the tools themselves so the surface a model reads is one
//! table rather than twenty-eight structs: a shape is edited here and nowhere
//! else.

use serde_json::{Value, json};

/// One upstream tool: the name the model calls it by, the description it
/// reads, and the argument schema the CLI validates against.
pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) schema: Value,
}

/// The whole surface, in upstream's own order.
pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "browser_close",
            description: "Close the page",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_resize",
            description: "Resize the browser window",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "width": {
                        "type": "number",
                        "description": "Width of the browser window"
                    },
                    "height": {
                        "type": "number",
                        "description": "Height of the browser window"
                    }
                },
                "required": ["width", "height"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_console_messages",
            description: "Returns all console messages",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "level": {
                        "default": "info",
                        "description": "Level of the console messages to return. Each level includes the messages of more severe levels. Defaults to \"info\".",
                        "type": "string",
                        "enum": ["error", "warning", "info", "debug"]
                    },
                    "all": {
                        "description": "Return all console messages since the beginning of the session, not just since the last navigation. Defaults to false.",
                        "type": "boolean"
                    },
                    "filename": {
                        "description": "File name to save the console messages to. Relative file names are resolved against the workspace root. If not provided, messages are returned as text.",
                        "type": "string"
                    }
                },
                "required": ["level"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_handle_dialog",
            description: "Handle a dialog",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "accept": {
                        "type": "boolean",
                        "description": "Whether to accept the dialog."
                    },
                    "promptText": {
                        "description": "The text of the prompt in case of a prompt dialog.",
                        "type": "string"
                    }
                },
                "required": ["accept"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_emulate_media",
            description: "Emulate CSS media features for the page, for example switch between the light and dark color scheme. Omitted parameters are left unchanged; null clears an override.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "colorScheme": {
                        "description": "Emulates the prefers-color-scheme media feature",
                        "anyOf": [
                            { "type": "string", "enum": ["light", "dark"] },
                            { "type": "null" }
                        ]
                    },
                    "reducedMotion": {
                        "description": "Emulates the prefers-reduced-motion media feature",
                        "anyOf": [
                            { "type": "string", "enum": ["reduce", "no-preference"] },
                            { "type": "null" }
                        ]
                    },
                    "forcedColors": {
                        "description": "Emulates the forced-colors media feature",
                        "anyOf": [
                            { "type": "string", "enum": ["active", "none"] },
                            { "type": "null" }
                        ]
                    },
                    "contrast": {
                        "description": "Emulates the prefers-contrast media feature",
                        "anyOf": [
                            { "type": "string", "enum": ["more", "no-preference"] },
                            { "type": "null" }
                        ]
                    },
                    "media": {
                        "description": "Changes the CSS media type of the page",
                        "anyOf": [
                            { "type": "string", "enum": ["screen", "print"] },
                            { "type": "null" }
                        ]
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_evaluate",
            description: "Evaluate JavaScript expression on page or element",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "description": "Exact target element reference from the page snapshot, or a unique element selector",
                        "type": "string"
                    },
                    "function": {
                        "type": "string",
                        "description": "() => { /* code */ } or (element) => { /* code */ } when element is provided"
                    },
                    "filename": {
                        "description": "File name to save the result to. Relative file names are resolved against the workspace root. If not provided, result is returned as text.",
                        "type": "string"
                    }
                },
                "required": ["function"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_file_upload",
            description: "Upload one or multiple files",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "paths": {
                        "description": "The absolute paths to the files to upload. Can be single file or multiple files. If omitted, file chooser is cancelled.",
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_drop",
            description: "Drop files or MIME-typed data onto an element, as if dragged from outside the page. At least one of \"paths\" or \"data\" must be provided.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    },
                    "paths": {
                        "description": "Absolute paths to files to drop onto the element.",
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "data": {
                        "description": "Data to drop, as a map of MIME type to string value (e.g. {\"text/plain\": \"hello\", \"text/uri-list\": \"https://example.com\"}).",
                        "type": "object",
                        "propertyNames": { "type": "string" },
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_find",
            description: "Search the accessibility snapshot of the current page for text or a regular expression. Returns matching snapshot nodes with a few lines of surrounding context (like search snippets), each shown under its path from the root of the tree, which is cheaper than capturing the whole snapshot when you only need to locate an element and its ref.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "text": {
                        "description": "Plain text to search for in the page snapshot (case-insensitive substring match). Provide either text or regex, not both.",
                        "type": "string"
                    },
                    "regex": {
                        "description": "Regular expression to search for in the page snapshot. Matching is case-sensitive by default; wrap the pattern in slashes to add flags, e.g. \"/error/i\" for case-insensitive. Provide either text or regex, not both.",
                        "type": "string"
                    },
                    "filename": {
                        "description": "Save results to a file instead of returning them in the response. Relative file names are resolved against the workspace root.",
                        "type": "string"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_fill_form",
            description: "Fill multiple form fields",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "element": {
                                    "description": "Human-readable element description used to obtain permission to interact with the element",
                                    "type": "string"
                                },
                                "target": {
                                    "type": "string",
                                    "description": "Exact target element reference from the page snapshot, or a unique element selector"
                                },
                                "name": {
                                    "type": "string",
                                    "description": "Human-readable field name"
                                },
                                "type": {
                                    "type": "string",
                                    "enum": ["textbox", "checkbox", "radio", "combobox", "slider"],
                                    "description": "Type of the field"
                                },
                                "value": {
                                    "type": "string",
                                    "description": "Value to fill in the field. If the field is a checkbox, the value should be `true` or `false`. If the field is a combobox, the value should be the text of the option."
                                }
                            },
                            "required": ["target", "name", "type", "value"],
                            "additionalProperties": false
                        },
                        "description": "Fields to fill in"
                    }
                },
                "required": ["fields"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_press_key",
            description: "Press a key on the keyboard",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Name of the key to press or a character to generate, such as `ArrowLeft` or `a`"
                    }
                },
                "required": ["key"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_type",
            description: "Type text into editable element",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type into the element"
                    },
                    "submit": {
                        "description": "Whether to submit entered text (press Enter after)",
                        "type": "boolean"
                    },
                    "slowly": {
                        "description": "Whether to type one character at a time. Useful for triggering key handlers in the page. By default entire text is filled in at once.",
                        "type": "boolean"
                    }
                },
                "required": ["target", "text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_navigate",
            description: "Navigate to a URL",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to navigate to"
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_navigate_back",
            description: "Go back to the previous page in the history",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_network_requests",
            description: "Returns a numbered list of network requests since loading the page. Use browser_network_request with the number to get full details.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "static": {
                        "default": false,
                        "description": "Whether to include successful static resources like images, fonts, scripts, etc. Defaults to false.",
                        "type": "boolean"
                    },
                    "filter": {
                        "description": "Only return requests whose URL matches this regexp (e.g. \"/api/.*user\").",
                        "type": "string"
                    },
                    "filename": {
                        "description": "File name to save the network requests to. Relative file names are resolved against the workspace root. If not provided, requests are returned as text.",
                        "type": "string"
                    }
                },
                "required": ["static"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_network_request",
            description: "Returns full details (headers and body) of a single network request, or a single part if `part` is set. Use the number from browser_network_requests.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "index": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 9_007_199_254_740_991_u64,
                        "description": "1-based index of the request, as printed by browser_network_requests."
                    },
                    "part": {
                        "description": "Return only this part of the request. Omit to return full details.",
                        "type": "string",
                        "enum": ["request-headers", "request-body", "response-headers", "response-body"]
                    },
                    "filename": {
                        "description": "File name to save the result to. Relative file names are resolved against the workspace root. If not provided, output is returned as text.",
                        "type": "string"
                    }
                },
                "required": ["index"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_run_code_unsafe",
            description: "Run a Playwright code snippet. Unsafe: executes arbitrary JavaScript in the Playwright server process and is RCE-equivalent.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "code": {
                        "description": "A JavaScript function containing Playwright code to execute. It will be invoked with a single argument, page, which you can use for any page interaction. For example: `async (page) => { await page.getByRole('button', { name: 'Submit' }).click(); return await page.title(); }`",
                        "type": "string"
                    },
                    "filename": {
                        "description": "Load code from the specified file. Relative file names are resolved against the workspace root. If both code and filename are provided, code will be ignored.",
                        "type": "string"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_take_screenshot",
            description: "Take a screenshot of the current page. You can't perform actions based on the screenshot, use browser_snapshot for actions.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "description": "Exact target element reference from the page snapshot, or a unique element selector",
                        "type": "string"
                    },
                    "type": {
                        "description": "Image format for the screenshot. If unset, inferred from the filename extension, otherwise png.",
                        "type": "string",
                        "enum": ["png", "jpeg", "webp"]
                    },
                    "filename": {
                        "description": "File name to save the screenshot to. Relative file names are resolved against the workspace root. If not specified, the screenshot is saved into the output directory as `page-{timestamp}.{png|jpeg|webp}`.",
                        "type": "string"
                    },
                    "fullPage": {
                        "description": "When true, takes a screenshot of the full scrollable page, instead of the currently visible viewport. Cannot be used with element screenshots.",
                        "type": "boolean"
                    },
                    "scale": {
                        "default": "css",
                        "description": "Image resolution scale. \"css\" produces a screenshot sized in CSS pixels (smaller, consistent across devices). \"device\" produces a high-resolution screenshot using device pixels (larger, accounts for the device pixel ratio). Default is css.",
                        "type": "string",
                        "enum": ["css", "device"]
                    }
                },
                "required": ["scale"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_snapshot",
            description: "Capture accessibility snapshot of the current page, this is better than screenshot",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "target": {
                        "description": "Exact target element reference from the page snapshot, or a unique element selector",
                        "type": "string"
                    },
                    "filename": {
                        "description": "Save snapshot to a file instead of returning it in the response. Relative file names are resolved against the workspace root.",
                        "type": "string"
                    },
                    "depth": {
                        "description": "Limit the depth of the snapshot tree",
                        "type": "number"
                    },
                    "boxes": {
                        "description": "Include each element's bounding box as [box=x,y,width,height] in the snapshot. Coordinates are viewport-relative, in CSS pixels (Element.getBoundingClientRect)",
                        "type": "boolean"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_click",
            description: "Perform click on a web page",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    },
                    "doubleClick": {
                        "description": "Whether to perform a double click instead of a single click",
                        "type": "boolean"
                    },
                    "button": {
                        "description": "Button to click, defaults to left",
                        "type": "string",
                        "enum": ["left", "right", "middle"]
                    },
                    "modifiers": {
                        "description": "Modifier keys to press",
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["Alt", "Control", "ControlOrMeta", "Meta", "Shift"]
                        }
                    },
                    // Beyond upstream: the actionability gate is what a
                    // headless click waits on, and a call that means "the
                    // element is there, click it anyway" has no other form.
                    "force": {
                        "description": "Click through the actionability gate with a real mouse event at the element's current position, instead of waiting for it to be visible, stable, enabled and uncovered.",
                        "type": "boolean"
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_drag",
            description: "Perform drag and drop between two elements",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "startElement": {
                        "description": "Human-readable source element description used to obtain the permission to interact with the element",
                        "type": "string"
                    },
                    "startTarget": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    },
                    "endElement": {
                        "description": "Human-readable target element description used to obtain the permission to interact with the element",
                        "type": "string"
                    },
                    "endTarget": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    }
                },
                "required": ["startTarget", "endTarget"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_hover",
            description: "Hover over element on page",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_select_option",
            description: "Select an option in a dropdown",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    },
                    "values": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Array of values to select in the dropdown. This can be a single value or multiple values."
                    }
                },
                "required": ["target", "values"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_tabs",
            description: "List, create, close, or select a browser tab.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "new", "close", "select"],
                        "description": "Operation to perform"
                    },
                    "index": {
                        "description": "Tab index, used for close/select. If omitted for close, current tab is closed.",
                        "type": "number"
                    },
                    "url": {
                        "description": "URL to navigate to in the new tab, used for new.",
                        "type": "string"
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_wait_for",
            description: "Wait for text to appear or disappear or a specified time to pass",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "time": {
                        "description": "The time to wait in seconds, at most 30",
                        "type": "number"
                    },
                    "text": {
                        "description": "The text to wait for",
                        "type": "string"
                    },
                    "textGone": {
                        "description": "The text to wait for to disappear",
                        "type": "string"
                    },
                    // Beyond upstream: a page whose readiness is a condition
                    // rather than a string has nothing here to wait on.
                    "expression": {
                        "description": "A JavaScript expression, evaluated repeatedly until it is truthy. Use this to wait for a condition no text can state, e.g. `document.querySelectorAll('.row').length > 3`.",
                        "type": "string"
                    }
                },
                "additionalProperties": false
            }),
        },
        // ------------------------------------------------------------------
        // Beyond upstream: tools built from the same driver, because the
        // question they answer is one a call cannot compose out of the 25.
        // ------------------------------------------------------------------
        ToolSpec {
            name: "browser_click_and_capture",
            description: "Click an element and return the network responses the click triggered - url, status and body - so a click whose meaning is its request does not need two calls and a poll in between.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector"
                    },
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    },
                    "settleMs": {
                        "description": "How long to keep collecting responses after the click before answering. Defaults to 1500 ms.",
                        "type": "number"
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "browser_form_state",
            description: "Read a form's state: every field's current value, whether it is marked invalid, and the text of the control itself - which is where a committed value lives for a picker that keeps it outside its input.",
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Exact target element reference from the page snapshot, or a unique element selector, for a form or a container of fields. Omit to read every form on the page."
                    },
                    "element": {
                        "description": "Human-readable element description used to obtain permission to interact with the element",
                        "type": "string"
                    }
                },
                "additionalProperties": false
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The surface is twenty-seven tools: the 25 upstream published and the
    /// two this family adds. A count is the cheap half; the shape of each is
    /// pinned below.
    #[test]
    fn the_surface_is_the_captures_tools_and_the_two_additions() {
        let names: Vec<&str> = specs().iter().map(|spec| spec.name).collect();
        assert_eq!(names.len(), 27, "25 upstream tools and two additions: {names:?}");
        for upstream in [
            "browser_close",
            "browser_resize",
            "browser_console_messages",
            "browser_handle_dialog",
            "browser_emulate_media",
            "browser_evaluate",
            "browser_file_upload",
            "browser_drop",
            "browser_find",
            "browser_fill_form",
            "browser_press_key",
            "browser_type",
            "browser_navigate",
            "browser_navigate_back",
            "browser_network_requests",
            "browser_network_request",
            "browser_run_code_unsafe",
            "browser_take_screenshot",
            "browser_snapshot",
            "browser_click",
            "browser_drag",
            "browser_hover",
            "browser_select_option",
            "browser_tabs",
            "browser_wait_for",
        ] {
            assert!(names.contains(&upstream), "{upstream} is missing from the surface");
        }
        assert!(
            names.contains(&"browser_click_and_capture") && names.contains(&"browser_form_state"),
            "the two beyond-upstream tools are on the surface: {names:?}",
        );
    }

    /// Every schema is an object schema that refuses arguments it does not
    /// know, and every name is the `browser_` prefix upstream's carry: a
    /// tool named otherwise is a prompt that resolves to something else.
    #[test]
    fn every_tool_is_a_closed_object_schema_with_upstreams_prefix() {
        for spec in specs() {
            assert!(spec.name.starts_with("browser_"), "{} is not a browser tool", spec.name);
            assert_eq!(spec.schema["type"], "object", "{} is not an object schema", spec.name);
            assert_eq!(
                spec.schema["additionalProperties"],
                json!(false),
                "{} accepts arguments it does not declare",
                spec.name,
            );
            assert!(
                !spec.description.is_empty(),
                "{} has no description for the model to read",
                spec.name,
            );
        }
    }

    /// The two arguments this family adds sit beside upstream's rather than
    /// replacing any: `force` on the click, `expression` on the wait.
    #[test]
    fn the_added_arguments_ride_tools_upstream_already_has() {
        let specs = specs();
        let click = specs.iter().find(|spec| spec.name == "browser_click").expect("browser_click");
        assert_eq!(
            click.schema["properties"]["force"]["type"],
            json!("boolean"),
            "the click carries `force`: {}",
            click.schema,
        );
        assert_eq!(click.schema["required"], json!(["target"]), "and requires what upstream does");

        let wait =
            specs.iter().find(|spec| spec.name == "browser_wait_for").expect("browser_wait_for");
        assert_eq!(
            wait.schema["properties"]["expression"]["type"],
            json!("string"),
            "the wait carries `expression`: {}",
            wait.schema,
        );
    }
}
