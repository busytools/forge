//! The browser family: the core `@playwright/mcp` tools, forwarded to the
//! one client connection that holds the browser role.
//!
//! **Names and shapes are upstream's, exactly.** A prompt written against a
//! Playwright MCP server names `browser_navigate` and passes a snapshot's
//! `target` ref, so the tools register unprefixed, with upstream's own
//! descriptions and argument schemas - transcribed from `@playwright/mcp`
//! 0.0.83's `tools/list`, the capture preserved with the V1 spec. Anything
//! this family added on top (a `context` name, a forced click) would be a
//! shape upstream does not have and a prompt that means something else
//! here.
//!
//! **The family is any-caller, and it is not a toggleable one.** The browser
//! is one machine-global client, not a scope like reviews or crons: every
//! session may drive it, and with no browser-capable client connected each
//! tool answers the named error rather than pretending the surface is
//! absent.
//!
//! **The tool carries no logic.** It forwards the call to the host and hands
//! back what the host returned - refs only mean something inside the client's
//! page state, so a server-side implementation of any of these is impossible
//! by construction, which is why the relay exists at all.

use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::browser::BrowserPart;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

#[cfg(test)]
use forge_sdk::mcp::server::McpServer;
use serde_json::{Value, json};

use crate::mcp::browser::facade::BrowserFacade;

pub mod facade;

/// One upstream tool: the name the model calls it by, the description it
/// reads, and the argument schema the CLI validates against.
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

/// The core tools, in the order this phase lists them - which is the
/// capture's order for the ones it takes.
///
/// The set is phase 1's: enough to prove the pipe by driving a page for real.
/// The rest of the 25 land with the full-surface phase, as their own
/// transcription of the same capture.
fn specs() -> Vec<ToolSpec> {
    vec![
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
                    }
                },
                "required": ["target"],
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
                    }
                },
                "additionalProperties": false
            }),
        },
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
    ]
}

/// Build a standalone `forge` MCP server carrying only the browser tools.
/// Test-only; production shares one `forge` server via
/// [`crate::mcp::build_forge_server`].
#[cfg(test)]
pub fn build_server(facade: Arc<dyn BrowserFacade>, slot: SessionSlot) -> McpServer {
    add_tools(McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")), facade, slot).build()
}

/// Attach the browser tools to an existing builder, so they share the
/// `forge` server name with every other family.
///
/// The last spec takes the caller's own handles: every tool keeps a handle of
/// its own, and nothing is left holding the originals once they are all
/// registered.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn BrowserFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let mut builder = builder;
    let mut specs = specs().into_iter().peekable();
    while let Some(spec) = specs.next() {
        if specs.peek().is_none() {
            return builder.tool(BrowserTool { spec, facade, slot });
        }
        builder =
            builder.tool(BrowserTool { spec, facade: Arc::clone(&facade), slot: slot.clone() });
    }
    builder
}

/// One registered tool: upstream's spec, plus the caller it acts for.
struct BrowserTool {
    spec: ToolSpec,
    facade: Arc<dyn BrowserFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &'static str {
        self.spec.name
    }

    fn description(&self) -> &'static str {
        self.spec.description
    }

    fn input_schema(&self) -> Value {
        self.spec.schema.clone()
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let started = std::time::Instant::now();
        let outcome = self.facade.call(&self.slot, self.spec.name, input.value).await;
        // The record of calls: what ran, for whom, and how it ended. `debug`
        // because a browser call is the session's own work rather than a
        // problem forge has.
        tracing::debug!(
            target: "forge_workspace::browser",
            event_name = "browser_call",
            tool = self.spec.name,
            slot = %self.slot.display(),
            ok = outcome.is_ok(),
            ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "a browser tool call returned",
        );
        match outcome {
            Ok(parts) => parts_output(parts),
            Err(why) => ToolOutput::error(why),
        }
    }
}

/// What a host's answer becomes on the tool's way back: one content block per
/// part, in order.
fn parts_output(parts: Vec<BrowserPart>) -> ToolOutput {
    let blocks = parts
        .into_iter()
        .map(|part| match part {
            BrowserPart::Text { text } => ToolOutputBlock::Text { text },
            BrowserPart::Image { mime_type, bytes } => {
                ToolOutputBlock::Image { mime_type, data: bytes }
            }
        })
        .collect();
    ToolOutput { blocks, is_error: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::browser::facade::MockBrowserFacade;

    fn seat() -> SessionSlot {
        SessionSlot::from_str_for_test("caller")
    }

    /// Every tool name the server registered, read off its debug listing.
    fn names(facade: Arc<dyn BrowserFacade>) -> Vec<String> {
        let debug = format!("{:?}", build_server(facade, seat()));
        let (_, tools) = debug.split_once("tools: [").expect("debug lists the tool names");
        let (tools, _) = tools.split_once(']').expect("the tool list is closed");
        tools
            .split(", ")
            .map(|name| name.trim_matches('"').to_owned())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// The registered surface is exactly upstream's 8 core tools, unprefixed:
    /// a prompt written against a Playwright MCP server names these, and a
    /// `browser__*` prefix or a renamed tool is a prompt that no longer
    /// resolves.
    ///
    /// Sorted rather than in upstream's order, because the server hands its
    /// tools out of a map: what is pinned here is the SET that resolves, and
    /// the order upstream lists them in is pinned by the shape test instead.
    #[test]
    fn the_core_tools_register_with_upstreams_names() {
        let mut registered = names(MockBrowserFacade::new().into_arc());
        registered.sort();
        assert_eq!(
            registered,
            [
                "browser_click",
                "browser_close",
                "browser_evaluate",
                "browser_navigate",
                "browser_run_code_unsafe",
                "browser_snapshot",
                "browser_type",
                "browser_wait_for",
            ],
        );
    }

    /// Each tool's argument shape is the one `@playwright/mcp` 0.0.83
    /// published: the property names and which of them are required. A
    /// dropped property is a call the CLI refuses before it is ever sent.
    #[test]
    fn each_tool_carries_upstreams_argument_shape() {
        // Transcribed from the 0.0.83 capture, independently of the specs
        // above: the two agreeing is the check, so a typo in one shows as a
        // disagreement rather than as a shape pinned twice.
        const EXPECTED: &[(&str, &[&str], &[&str])] = &[
            ("browser_navigate", &["url"], &["url"]),
            ("browser_snapshot", &["target", "filename", "depth", "boxes"], &[]),
            (
                "browser_click",
                &["element", "target", "doubleClick", "button", "modifiers"],
                &["target"],
            ),
            (
                "browser_type",
                &["element", "target", "text", "submit", "slowly"],
                &["target", "text"],
            ),
            ("browser_evaluate", &["element", "target", "function", "filename"], &["function"]),
            ("browser_run_code_unsafe", &["code", "filename"], &[]),
            ("browser_wait_for", &["time", "text", "textGone"], &[]),
            ("browser_close", &[], &[]),
        ];
        let specs = specs();
        assert_eq!(specs.len(), EXPECTED.len(), "the expected table covers every core tool");

        for (spec, (name, properties, required)) in specs.iter().zip(EXPECTED) {
            assert_eq!(spec.name, *name, "the table is in the tools' own order");
            let schema = &spec.schema;
            let mut got: Vec<&str> = schema["properties"]
                .as_object()
                .expect("a schema describes an object")
                .keys()
                .map(String::as_str)
                .collect();
            got.sort_unstable();
            let mut want: Vec<&str> = properties.to_vec();
            want.sort_unstable();
            assert_eq!(got, want, "{name}: the argument names are upstream's");

            let got_required: Vec<&str> = schema["required"]
                .as_array()
                .map_or_else(Vec::new, |list| list.iter().filter_map(Value::as_str).collect());
            assert_eq!(got_required, *required, "{name}: the required arguments are upstream's");
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{name}: upstream refuses arguments it does not know, and so does this",
            );
        }
    }

    /// A call reaches the host with the arguments the CLI sent, and the
    /// caller's own slot rides along - the client shows who drives what.
    #[tokio::test]
    async fn a_call_carries_the_arguments_and_the_callers_slot() {
        let mock = Arc::new(MockBrowserFacade::new());
        let tool = BrowserTool {
            spec: specs().remove(0),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({"url": "https://example.com"}) }).await;

        assert!(!out.is_error, "the mock's answer is a result: {:?}", out.blocks);
        assert_eq!(
            mock.calls.lock().as_slice(),
            [("browser_navigate".to_owned(), json!({"url": "https://example.com"}))],
            "the host is asked for the tool the model called, with its arguments verbatim",
        );
        assert_eq!(
            out.blocks,
            vec![ToolOutputBlock::Text { text: "done".to_owned() }],
            "and the host's parts are what the tool returns",
        );
    }

    /// No client holding the role is a tool error naming the reason, not an
    /// empty result or a hang.
    #[tokio::test]
    async fn a_refused_call_is_a_tool_error_naming_the_reason() {
        let mock = Arc::new(MockBrowserFacade::new());
        *mock.answer.lock() = Err(crate::browser::NO_BROWSER_CLIENT.to_owned());
        let tool = BrowserTool {
            spec: specs().remove(0),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({"url": "https://example.com"}) }).await;

        assert!(out.is_error, "the refusal is the call's failure");
        assert_eq!(
            out.blocks,
            vec![ToolOutputBlock::Text { text: crate::browser::NO_BROWSER_CLIENT.to_owned() }],
            "and it carries the reason a session can act on",
        );
    }

    /// An image the host answered with crosses as an MCP image block, bytes
    /// and all: a screenshot is a result, not a path a client has to go and
    /// open.
    #[tokio::test]
    async fn an_image_answer_crosses_as_an_image_block() {
        let mock = Arc::new(MockBrowserFacade::new());
        *mock.answer.lock() = Ok(vec![
            BrowserPart::Text { text: "captured".to_owned() },
            BrowserPart::Image { mime_type: "image/png".to_owned(), bytes: vec![1, 2] },
        ]);
        let tool = BrowserTool {
            spec: specs().remove(1),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({}) }).await;

        assert_eq!(
            out.blocks,
            vec![
                ToolOutputBlock::Text { text: "captured".to_owned() },
                ToolOutputBlock::Image { mime_type: "image/png".to_owned(), data: vec![1, 2] },
            ],
            "the parts cross in order, the image's bytes with them",
        );
    }

    /// **The whole seam, from a tool call to the channel the workspace hands
    /// out.** A tool driven through the built server is answered by a host
    /// registered through `Workspace::browser_relay()` - the same accessor the
    /// spawn builds its facade from - so a facade wired to a relay of its own
    /// fails here rather than shipping as every browser tool answering "no
    /// browser-capable client connected" while a client sits attached.
    #[tokio::test]
    async fn a_tool_driven_through_the_server_asks_the_relay_the_workspace_hands_out() {
        use crate::mcp::browser::facade::ProdBrowserFacade;
        use crate::mcp::cron::facade::MockCronFacade;
        use crate::mcp::gotify::facade::MockGotifyFacade;
        use crate::mcp::peers::facade::MockWorkspaceFacade;
        use crate::mcp::review::facade::MockReviewFacade;
        use crate::mcp::slack::facade::MockSlackFacade;
        use crate::mcp::tasks::facade::MockTasksFacade;
        use crate::mcp::workers::facade::MockWorkerFacade;
        use tokio::sync::mpsc;

        let (workspace, _updates) = crate::workspace::Workspace::testing_stub();
        let (to_host, mut asks) = mpsc::unbounded_channel();
        assert!(
            workspace.browser_relay().register(1, to_host),
            "precondition: the role is free on a fresh workspace",
        );

        let server = crate::mcp::build_forge_server(
            crate::mcp::ForgeServerFacades {
                workspace: MockWorkspaceFacade::new().into_arc(),
                worker: MockWorkerFacade::new().into_arc(),
                // The exact composition the spawn uses.
                browser: ProdBrowserFacade::from_relay(workspace.browser_relay()),
                review: MockReviewFacade::new().into_arc(),
                cron: MockCronFacade::new().into_arc(),
                gotify: MockGotifyFacade::new().into_arc(),
                slack: MockSlackFacade::new().into_arc(),
                tasks: MockTasksFacade::new().into_arc(),
                systemone: None,
            },
            &crate::mcp::McpFamily::all(),
            seat(),
            crate::mcp::SessionKind::Worker,
        );

        let host = tokio::spawn(async move {
            let request = asks.recv().await.expect("the ask reached the registered host");
            assert_eq!(request.tool, "browser_close", "and names the tool that was called");
            request.reply.send(Ok(vec![BrowserPart::Text { text: "closed".to_owned() }])).ok();
        });

        let answer = server
            .dispatch(&forge_sdk::mcp::protocol::JsonRpcRequest {
                jsonrpc: "2.0".to_owned(),
                id: Some(json!(1)),
                method: "tools/call".to_owned(),
                params: Some(json!({ "name": "browser_close", "arguments": {} })),
            })
            .await
            .expect("a tools/call is answered");
        // Bounded, so a facade wired to a relay of its own fails here -
        // naming what never happened - rather than holding the run open.
        tokio::time::timeout(std::time::Duration::from_secs(5), host)
            .await
            .expect(
                "the ask reached the registered host rather than the tool answering without one",
            )
            .expect("the host task ran");

        let encoded = format!("{answer:?}");
        assert!(
            encoded.contains("closed"),
            "the tool's answer is the host's parts, so the call reached it: {encoded}",
        );
        assert!(!encoded.contains("no browser-capable client"), "{encoded}");
    }
}
