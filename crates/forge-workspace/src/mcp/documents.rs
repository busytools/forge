//! The document hand-off family: one tool a session calls to hand the person
//! at a client a document - markdown, a PDF or plain text - which opens in
//! the client's own viewer rather than in the conversation.
//!
//! **The tool answers at the reader's finish, like the browser hand-off.**
//! The call parks on the ask - **open** or **not now** - and the viewer's
//! done releases it with what the finish filed; a review the reader leaves
//! is filed under the document's own scope and named in the answer, so the
//! agent reads it back like any review. `NotNow` ends the call with the
//! reader's decline, and a client that goes away ends it abandoned.
//!
//! **Any-caller and not toggleable, like the browser**: every session may
//! hand a document out of its own tree, and the read resolves against that
//! tree - so there is nothing a project could withhold. Every state the read
//! can refuse is the call's failure, with the read's own sentence in it.

use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::document::DocumentHandOffEnding;
use forge_primitives::review::doc_scope_ref;
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
their review then arrives filed under the document's own scope. A PDF \
always opens read-only. The reader is asked to open it or not, and **this \
call waits for them**: it answers when they finish - with the review their \
comments were filed as - or when they say not now.";

/// The one tool: the hand-off itself.
pub(crate) struct HandOffTool {
    facade: Arc<dyn DocumentFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for HandOffTool {
    fn name(&self) -> &'static str {
        // The family segment is the family: every forge tool is
        // `mcp__forge__<family>__<verb>`, and the client reads the card and
        // glyph out of that segment - a tool without one draws as the
        // generic MCP call (Ved, the look: "every tool has its own glyph").
        "documents__hand_off"
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
                    "description": "Ask the reader for a review of this document. A document a \
                                    client can read offers its review either way; a PDF always \
                                    opens read-only.",
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
            return Err("documents__hand_off needs `path`: the document's path, relative to this \
                 session's working directory"
                .to_owned());
        };
        let reviewable = match args.get("reviewable") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(other) => return Err(format!("`reviewable` is true or false, not {other}")),
        };
        let (resolved, ending) = self.facade.hand_off(&self.slot, path, reviewable).await?;
        Ok(match ending {
            DocumentHandOffEnding::Done { review_id: Some(review_id) } => format!(
                "The reader finished at {resolved}, and their comments are filed as the review \
                 {review_id} under the document's own scope, {} - read them back with \
                 review__get and pass that scope.",
                doc_scope_ref(&resolved),
            ),
            DocumentHandOffEnding::Done { review_id: None } => {
                format!("The reader finished at {resolved} without leaving comments.")
            }
            DocumentHandOffEnding::NotNow => {
                return Err("the reader said not now, so nothing opened".to_owned());
            }
            DocumentHandOffEnding::Abandoned => {
                return Err(
                    "the reader's client went away before they finished, so the hand-off ended \
                     without an answer"
                        .to_owned(),
                );
            }
        })
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
    /// resolved path and the ending the park released on.
    #[tokio::test]
    async fn a_hand_off_carries_the_path_and_the_flag_for_the_callers_seat() {
        let mock = Arc::new(MockDocumentFacade::new());
        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Done { review_id: None },
        ));

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

    /// A finish that filed a review names the scope it landed under and the
    /// review's own id, so the agent reads it back without a failed call
    /// first; a finish with nothing commented names neither.
    #[tokio::test]
    async fn a_finished_hand_offs_answer_names_the_review_and_its_scope() {
        let mock = Arc::new(MockDocumentFacade::new());
        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Done { review_id: Some("r-7".to_owned()) },
        ));
        let tool = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>);

        let with_review = tool.call(ToolInput { value: json!({ "path": "docs/readme.md" }) }).await;
        assert!(!with_review.is_error, "{:?}", with_review.blocks);
        let text = crate::mcp::test_support::block_text(&with_review.blocks[0]);
        assert!(text.contains("r-7"), "the answer names the review: {text}");
        assert!(text.contains("doc:/proj/docs/readme.md"), "and the scope: {text}");

        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Done { review_id: None },
        ));
        let without = tool.call(ToolInput { value: json!({ "path": "docs/readme.md" }) }).await;
        assert!(!without.is_error, "{:?}", without.blocks);
        let text = crate::mcp::test_support::block_text(&without.blocks[0]);
        assert!(!text.contains("doc:"), "nothing commented, so no scope to name: {text}");
    }

    /// The reader's decline and an abandoned client are the call's failures,
    /// each with its own sentence - the agent must not read either as done.
    #[tokio::test]
    async fn a_declined_or_abandoned_hand_off_fails_with_its_reason() {
        let mock = Arc::new(MockDocumentFacade::new());
        let tool = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>);

        *mock.answer.lock() =
            Ok(("/proj/docs/readme.md".to_owned(), DocumentHandOffEnding::NotNow));
        let out = tool.call(ToolInput { value: json!({ "path": "docs/readme.md" }) }).await;
        assert!(out.is_error, "a decline is not a success: {:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("not now"), "{text}");

        *mock.answer.lock() =
            Ok(("/proj/docs/readme.md".to_owned(), DocumentHandOffEnding::Abandoned));
        let out = tool.call(ToolInput { value: json!({ "path": "docs/readme.md" }) }).await;
        assert!(out.is_error, "an abandoned hand-off is not a success: {:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("went away"), "{text}");
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
    fn the_family_registers_one_tool_named_documents_hand_off() {
        let server = build_server(MockDocumentFacade::new().into_arc(), seat());
        let debug = format!("{server:?}");
        let (_, tools) = debug.split_once("tools: [").expect("debug lists the tool names");
        let (tools, _) = tools.split_once(']').expect("the tool list is closed");
        let names: Vec<&str> = tools
            .split(", ")
            .map(|name| name.trim_matches('"'))
            .filter(|n| !n.is_empty())
            .collect();
        assert_eq!(names, ["documents__hand_off"]);
    }
}
