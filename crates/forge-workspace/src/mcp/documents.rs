//! The document hand-off family: one tool a session calls to hand the person
//! at a client a document - markdown, a PDF or plain text - which opens in
//! the client's own viewer rather than in the conversation.
//!
//! **The tool answers at the open.** The hand-off's two moments split there:
//! this call returns with the resolved path once the viewer is raised, and a
//! review the reader leaves arrives later, as the same event a code review
//! raises. Nothing parks on the reader's pace.
//!
//! **Any-caller and not toggleable, like the browser**: every session may
//! hand a document out of its own tree, and the read resolves against that
//! tree - so there is nothing a project could withhold. Every state the read
//! can refuse is the call's failure, with the read's own sentence in it.

use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
use serde_json::Value;

use crate::mcp::documents::facade::DocumentFacade;

pub mod facade;

/// Build a standalone `forge` MCP server carrying only the hand-off tool.
/// Test-only; production shares one `forge` server via
/// [`crate::mcp::build_forge_server`].
#[cfg(test)]
pub fn build_server(
    facade: Arc<dyn DocumentFacade>,
    slot: SessionSlot,
) -> forge_sdk::mcp::server::McpServer {
    add_tools(McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")), facade, slot).build()
}

/// Attach the hand-off tool to an existing builder, so it shares the `forge`
/// server name with every other family.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn DocumentFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    builder.tool(HandOffTool { facade, slot })
}

/// What the model reads about the tool before calling it: what it hands
/// over, when `reviewable` earns its place, and which moment this answers.
const DESCRIPTION: &str = "\
Hand the reader a document from this session's working tree, opened in their \
own viewer. Use it when a document is the thing the reader should see - a \
plan, a report, a spec - rather than part of the conversation. `path` is \
relative to the working directory. Set `reviewable` when you want review \
comments back: the reader opens it, comments on lines and finishes, and \
their review then arrives like a code review. A PDF always opens read-only. \
This answers when the document opens, not when the reading is done - \
nothing waits on the reader.";

/// The one tool: the hand-off itself.
pub(crate) struct HandOffTool {
    facade: Arc<dyn DocumentFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for HandOffTool {
    fn name(&self) -> &'static str {
        "hand_off_document"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The document's path, relative to the session's working \
                                    directory.",
                },
                "reviewable": {
                    "type": "boolean",
                    "description": "Whether the reader can leave review comments on it. Off by \
                                    default; a PDF always opens read-only.",
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let started = std::time::Instant::now();
        let outcome = self.hand_off(input.value).await;
        // The record of calls: what was handed, for whom, and how it ended.
        // `debug` because a hand-off is the session's own work rather than a
        // problem forge has.
        tracing::debug!(
            target: "forge_workspace::documents",
            event_name = "document_hand_off",
            slot = %self.slot.display(),
            ok = outcome.is_ok(),
            ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "a document hand-off returned",
        );
        match outcome {
            Ok(text) => {
                ToolOutput { blocks: vec![ToolOutputBlock::Text { text }], is_error: false }
            }
            Err(why) => ToolOutput::error(why),
        }
    }
}

impl HandOffTool {
    async fn hand_off(&self, args: Value) -> Result<String, String> {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Err(
                "hand_off_document needs `path`: the document's path, relative to this session's \
                 working directory"
                    .to_owned(),
            );
        };
        let reviewable = match args.get("reviewable") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(other) => return Err(format!("`reviewable` is true or false, not {other}")),
        };
        let resolved = self.facade.hand_off(&self.slot, path, reviewable).await?;
        Ok(format!(
            "The document is open in the reader's viewer at {resolved}. If the reader leaves \
             review comments, they arrive later as a review left; this answer is the open, not \
             the finish."
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::SessionSlot;
    use serde_json::json;

    use super::*;
    use crate::mcp::documents::facade::{DocumentFacade, MockDocumentFacade};

    fn seat() -> SessionSlot {
        SessionSlot::from_str_for_test("caller")
    }

    fn tool(facade: Arc<dyn DocumentFacade>) -> HandOffTool {
        HandOffTool { facade, slot: seat() }
    }

    /// The whole call: the path and the reviewable flag cross to the
    /// hand-off for the caller's own seat, and the answer carries the
    /// resolved path - the open, not the finish.
    #[tokio::test]
    async fn a_hand_off_carries_the_path_and_the_flag_for_the_callers_seat() {
        let mock = Arc::new(MockDocumentFacade::new());
        *mock.answer.lock() = Ok("/proj/docs/readme.md".to_owned());

        let out = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>)
            .call(ToolInput { value: json!({ "path": "docs/readme.md", "reviewable": true }) })
            .await;

        assert!(!out.is_error, "{:?}", out.blocks);
        assert_eq!(
            mock.calls.lock().as_slice(),
            [("docs/readme.md".to_owned(), true)],
            "the path and the flag cross verbatim",
        );
        assert_eq!(
            mock.seats.lock().as_slice(),
            [seat()],
            "and for the seat that called, so the path cannot resolve against another tree",
        );
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("/proj/docs/readme.md"), "the answer carries the resolved path");
    }

    /// A refused read is the call's failure, with the read's own sentence in
    /// it - the same one a viewer draws.
    #[tokio::test]
    async fn a_refused_read_is_a_tool_error_with_the_reads_reason() {
        let mock = Arc::new(MockDocumentFacade::new());
        *mock.answer.lock() =
            Err("the path resolves outside the session's tree: ../../etc/passwd".to_owned());

        let out = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>)
            .call(ToolInput { value: json!({ "path": "../../etc/passwd" }) })
            .await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("outside the session's tree"), "{text}");
    }

    /// `reviewable` is off unless asked for: everything the agent did not
    /// flag opens read-only.
    #[tokio::test]
    async fn reviewable_is_off_unless_asked_for() {
        let mock = Arc::new(MockDocumentFacade::new());

        let out = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>)
            .call(ToolInput { value: json!({ "path": "notes.md" }) })
            .await;

        assert!(!out.is_error, "{:?}", out.blocks);
        assert_eq!(
            mock.calls.lock().as_slice(),
            [("notes.md".to_owned(), false)],
            "an absent flag reads as read-only",
        );
    }

    /// No path is the call's own mistake, answered before anything reads.
    #[tokio::test]
    async fn a_hand_off_without_a_path_is_refused() {
        let mock = Arc::new(MockDocumentFacade::new());

        let out = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>)
            .call(ToolInput { value: json!({}) })
            .await;

        assert!(out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("`path`"), "{text}");
        assert!(mock.calls.lock().is_empty(), "and nothing read");
    }

    /// The family registers exactly one tool, under the name a prompt calls
    /// it by.
    #[test]
    fn the_family_registers_one_tool_named_hand_off_document() {
        let server = build_server(MockDocumentFacade::new().into_arc(), seat());
        let debug = format!("{server:?}");
        let (_, tools) = debug.split_once("tools: [").expect("debug lists the tool names");
        let (tools, _) = tools.split_once(']').expect("the tool list is closed");
        let names: Vec<&str> = tools
            .split(", ")
            .map(|name| name.trim_matches('"'))
            .filter(|n| !n.is_empty())
            .collect();
        assert_eq!(names, ["hand_off_document"]);
    }
}
