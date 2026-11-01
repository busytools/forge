//! The browser family: the core `@playwright/mcp` tools, forwarded to the
//! server's browser fleet.
//!
//! **Names and argument shapes are upstream's, exactly.** A prompt written
//! against a Playwright MCP server names `browser_navigate` and passes a
//! snapshot's `target` ref, so the tools register unprefixed, with upstream's
//! argument schemas - transcribed from `@playwright/mcp` 0.0.83's
//! `tools/list`, the capture preserved with the V1 spec. Anything this family
//! added on top (a hand-off `tab`, a forced click) would be a shape upstream
//! does not have and a prompt that means something else here. Two
//! descriptions differ: the close's is this family's own, because a close
//! here closes the session's current tab, and the snapshot's gains the
//! sentences on a ref's lifetime and on driving a page that keeps changing.
//!
//! **The family is any-caller, and it is not a toggleable one.** The browser
//! is one machine-global one, not a scope like reviews or crons: every
//! session may drive it, and a workspace with no fleet answers the no-host
//! refusal rather than pretending the surface is absent.
//!
//! **The tool carries no logic.** It forwards the call to the fleet and
//! hands back the parts it returned - refs only mean something inside that
//! browser's own page state, so the server drives the page rather than
//! answering for it.

use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserPart, HandOffEnding};
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

#[cfg(test)]
use forge_sdk::mcp::server::McpServer;
use serde_json::Value;

use crate::mcp::browser::facade::BrowserFacade;

pub mod facade;

pub mod specs;

use specs::ToolSpec;

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
    let mut specs = specs::specs().into_iter().peekable();
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
        let outcome = if self.spec.name == "browser_hand_off" {
            self.hand_off(input.value).await
        } else {
            self.facade.call(&self.slot, self.spec.name, input.value).await
        };
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

impl BrowserTool {
    /// **The one tool whose logic is the core's own.** Every other tool
    /// forwards to the host because a `target` ref only means something in
    /// the client's page state; the hand-off parks on the person, and the
    /// wait - with no timeout - is the answer.
    async fn hand_off(&self, args: Value) -> Result<Vec<BrowserPart>, String> {
        // **An ask with no words is no ask**: a blank reason parks on the
        // person a prompt that says nothing, so it is refused here rather
        // than drawn as an empty row.
        let Some(reason) =
            args.get("reason").and_then(Value::as_str).filter(|reason| !reason.trim().is_empty())
        else {
            return Err("browser_hand_off needs `reason`: what the person should do".to_owned());
        };
        let Some(tab) =
            args.get("tab").and_then(Value::as_str).filter(|tab| !tab.trim().is_empty())
        else {
            return Err(
                "browser_hand_off needs `tab`: the page id of one of this session's tabs, from \
                 browser_tabs list"
                    .to_owned(),
            );
        };
        match self.facade.hand_off(&self.slot, reason, tab).await? {
            HandOffEnding::Done => Ok(vec![BrowserPart::Text {
                text: "The person is done in the browser; carry on from where you left off."
                    .to_owned(),
            }]),
            HandOffEnding::NotNow => Err(
                "The person declined to act for now; the browser is as it was - decide what to \
                 do instead."
                    .to_owned(),
            ),
            // **The person is still on the page**: they sent their words as
            // a prompt beside this result, so the answer names that rather
            // than reading as a decline.
            HandOffEnding::Feedback => Ok(vec![BrowserPart::Text {
                text: "The person sent you a message about the page instead of Done or Not \
                       now; read it and carry on - the browser is as it was."
                    .to_owned(),
            }]),
            // The waiter cannot outlive a drop that sends this, so seeing it
            // here would mean the registry and the waiter disagree.
            HandOffEnding::Abandoned => Err("The hand-off ended without an answer.".to_owned()),
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
    use serde_json::json;

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

    /// The registered surface is the whole table, UNPREFIXED: a prompt
    /// written against a Playwright MCP server names `browser_navigate`, and
    /// a `browser__*` prefix or a renamed tool is a prompt that no longer
    /// resolves. Every name the table carries registers, and nothing else
    /// does.
    ///
    /// Sorted rather than in upstream's order, because the server hands its
    /// tools out of a map: what is pinned here is the SET that resolves,
    /// against the table read as a set too, so a tool added to one and not
    /// the other fails.
    #[test]
    fn the_whole_surface_registers_with_the_tables_names() {
        let mut registered = names(MockBrowserFacade::new().into_arc());
        registered.sort();
        let mut expected: Vec<&str> = specs::specs().iter().map(|spec| spec.name).collect();
        expected.sort_unstable();
        assert_eq!(
            registered, expected,
            "the registered names are the table's, and nothing else's"
        );
        assert!(
            registered.iter().all(|name| !name.contains("__")),
            "every browser tool crosses unprefixed: {registered:?}",
        );
    }

    /// **The three shapes the live capture corrected**, and the arguments
    /// this family adds beside upstream's - each transcribed independently
    /// of the table in [`specs`], so the two agreeing is the check rather
    /// than a shape pinned twice. A dropped argument here is a call the CLI
    /// refuses before it is ever sent; an extra one upstream does not have is
    /// a prompt that means something else here.
    #[test]
    fn the_corrected_shapes_and_the_added_arguments_are_what_the_capture_says() {
        const EXPECTED: &[(&str, &[&str], &[&str])] = &[
            // `scale` is REQUIRED on a screenshot, which the prose
            // transcription missed and the live schema does not.
            (
                "browser_take_screenshot",
                &["element", "target", "type", "filename", "fullPage", "scale"],
                &["scale"],
            ),
            // `filename` is on evaluate, which the prose transcription missed.
            ("browser_evaluate", &["element", "target", "function", "filename"], &["function"]),
            // And run_code_unsafe marks nothing required.
            ("browser_run_code_unsafe", &["code", "filename"], &[]),
            // Beyond upstream, on tools upstream already has.
            (
                "browser_click",
                &["element", "target", "doubleClick", "button", "modifiers", "force"],
                &["target"],
            ),
            ("browser_wait_for", &["time", "text", "textGone", "expression"], &[]),
        ];
        let all = specs::specs();
        for (name, properties, required) in EXPECTED {
            let spec = all
                .iter()
                .find(|spec| spec.name == *name)
                .unwrap_or_else(|| panic!("{name} is not on the surface"));
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
            assert_eq!(got, want, "{name}: the argument names are the capture's");

            let got_required: Vec<&str> = schema["required"]
                .as_array()
                .map_or_else(Vec::new, |list| list.iter().filter_map(Value::as_str).collect());
            assert_eq!(got_required, *required, "{name}: the required arguments are the capture's");
        }
    }

    /// One tool of the table, by name: a test that names the tool it drives
    /// cannot be moved onto another one by a reordering of the surface.
    fn spec_named(name: &str) -> ToolSpec {
        specs::specs()
            .into_iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("{name} is not on the surface"))
    }

    /// A call reaches the host with the arguments the CLI sent, and the
    /// caller's own slot rides along - the client shows who drives what.
    #[tokio::test]
    async fn a_call_carries_the_arguments_and_the_callers_slot() {
        let mock = Arc::new(MockBrowserFacade::new());
        let tool = BrowserTool {
            spec: spec_named("browser_navigate"),
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
            mock.seats.lock().as_slice(),
            [seat()],
            "and for the seat that called, so a wrong slot cannot show the wrong session as the asker",
        );
        assert_eq!(
            out.blocks,
            vec![ToolOutputBlock::Text { text: "done".to_owned() }],
            "and the host's parts are what the tool returns",
        );
    }

    /// A refused call is a tool error naming the reason, not an empty result
    /// or a hang.
    #[tokio::test]
    async fn a_refused_call_is_a_tool_error_naming_the_reason() {
        let mock = Arc::new(MockBrowserFacade::new());
        *mock.answer.lock() = Err(crate::browser::NO_BROWSER_HOST.to_owned());
        let tool = BrowserTool {
            spec: spec_named("browser_navigate"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({"url": "https://example.com"}) }).await;

        assert!(out.is_error, "the refusal is the call's failure");
        assert_eq!(
            out.blocks,
            vec![ToolOutputBlock::Text { text: crate::browser::NO_BROWSER_HOST.to_owned() }],
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
            spec: spec_named("browser_take_screenshot"),
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

    /// **The whole seam, from a tool call to the fleet the server installs.**
    /// A tool driven through the built server is answered by the host
    /// installed on the workspace, so a facade reading a fleet of its own
    /// fails here rather than shipping as every browser tool answering the
    /// no-host refusal while a fleet runs beside it.
    #[tokio::test]
    async fn a_tool_driven_through_the_server_reaches_the_installed_host() {
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
        let (calls, mut heard) = mpsc::unbounded_channel();
        workspace
            .install_browser_host(std::sync::Arc::new(crate::browser::testing::StubHost { calls }));

        let server = crate::mcp::build_forge_server(
            crate::mcp::ForgeServerFacades {
                workspace: MockWorkspaceFacade::new().into_arc(),
                worker: MockWorkerFacade::new().into_arc(),
                // The exact composition the spawn uses.
                browser: ProdBrowserFacade::from_workspace(&workspace),
                documents: crate::mcp::documents::facade::ProdDocumentFacade::from_workspace(
                    &workspace,
                ),
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

        let answer = server
            .dispatch(&forge_sdk::mcp::protocol::JsonRpcRequest {
                jsonrpc: "2.0".to_owned(),
                id: Some(json!(1)),
                method: "tools/call".to_owned(),
                params: Some(json!({ "name": "browser_close", "arguments": {} })),
            })
            .await
            .expect("a tools/call is answered");

        let heard = heard.try_recv().expect(
            "the call reached the installed host rather than the tool answering without one",
        );
        assert_eq!(heard.0, "browser_close", "and names the tool that was called");
        let encoded = format!("{answer:?}");
        assert!(
            encoded.contains("clicked"),
            "the tool's answer is the host's parts, so the call reached it: {encoded}",
        );
        assert!(!encoded.contains("no browser-capable client"), "{encoded}");
    }

    /// The hand-off is the one tool that does not forward: it parks on the
    /// person, and what comes back to the model is the ending - Done means
    /// carry on.
    #[tokio::test]
    async fn a_hand_off_parks_on_the_person_and_a_done_answers_it() {
        let mock = Arc::new(MockBrowserFacade::new());
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool
            .call(ToolInput { value: json!({ "reason": "solve the CAPTCHA", "tab": "T3" }) })
            .await;

        assert!(!out.is_error, "{:?}", out.blocks);
        assert!(
            mock.calls.lock().is_empty(),
            "the hand-off never reaches the browser: the person is the ask, not the tool",
        );
        assert_eq!(
            mock.hand_offs.lock().as_slice(),
            [("solve the CAPTCHA".to_owned(), "T3".to_owned())],
            "the reason and the tab cross to the park verbatim",
        );
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("carry on"), "Done tells the model to carry on: {text}");
    }

    /// A decline is a tool error with the person's answer in it, so the model
    /// reads it as "not this way" rather than as a success.
    #[tokio::test]
    async fn a_hand_off_decline_is_a_tool_error() {
        let mock = Arc::new(MockBrowserFacade::new());
        *mock.hand_off_answer.lock() = Ok(HandOffEnding::NotNow);
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({ "reason": "sign in", "tab": "T2" }) }).await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("declined"), "{text}");
    }

    /// **A message from the person is not a refusal**, so it is not a tool
    /// error and does not tell the model to decide what to do instead: the
    /// person's words are a prompt riding beside this result, and the answer
    /// names them as one.
    #[tokio::test]
    async fn a_hand_off_feedback_is_not_a_decline() {
        let mock = Arc::new(MockBrowserFacade::new());
        *mock.hand_off_answer.lock() = Ok(HandOffEnding::Feedback);
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({ "reason": "sign in", "tab": "T2" }) }).await;

        assert!(!out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("sent you a message"), "the person spoke: {text}");
        assert!(!text.contains("declined"), "and the answer never reads as a decline: {text}");
    }

    /// No tab is the call's own mistake too: the person is asked to look at
    /// one tab, and a hand-off that named none would raise the wrong page.
    #[tokio::test]
    async fn a_hand_off_without_a_tab_is_refused() {
        let mock = Arc::new(MockBrowserFacade::new());
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({ "reason": "sign in" }) }).await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("`tab`"), "{text}");
        assert!(mock.hand_offs.lock().is_empty(), "and nothing was parked");
    }

    /// A blank reason is no reason: it would park an ask that says nothing to
    /// the person, so the call is refused before anything reaches them.
    #[tokio::test]
    async fn a_hand_off_with_a_blank_reason_is_refused() {
        let mock = Arc::new(MockBrowserFacade::new());
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({ "reason": "   ", "tab": "T2" }) }).await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("`reason`"), "{text}");
        assert!(mock.hand_offs.lock().is_empty(), "and nothing was parked");
    }

    /// No reason is the call's own mistake, answered before anything parks.
    #[tokio::test]
    async fn a_hand_off_without_a_reason_is_refused() {
        let mock = Arc::new(MockBrowserFacade::new());
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({}) }).await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("`reason`"), "{text}");
        assert!(mock.hand_offs.lock().is_empty(), "and nothing was parked");
    }

    /// A stream nobody can answer on fails closed with the named reason
    /// rather than parking the session on a prompt no view can draw.
    #[tokio::test]
    async fn a_hand_off_that_cannot_be_shown_fails_closed() {
        let mock = Arc::new(MockBrowserFacade::new());
        *mock.hand_off_answer.lock() =
            Err("no attached client can show the browser hand-off".to_owned());
        let tool = BrowserTool {
            spec: spec_named("browser_hand_off"),
            facade: Arc::clone(&mock) as Arc<dyn BrowserFacade>,
            slot: seat(),
        };

        let out = tool.call(ToolInput { value: json!({ "reason": "sign in", "tab": "T2" }) }).await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("no attached client"), "{text}");
    }
}
