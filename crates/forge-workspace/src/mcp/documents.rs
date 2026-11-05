//! The document hand-off family: one tool a session calls to hand the person
//! at a client a document - markdown, a PDF or plain text - which opens in
//! the client's own viewer rather than in the conversation.
//!
//! **The tool answers at the reader's decision, like the browser hand-off.**
//! The call parks on the ask - **open** or **not now** - and the viewer's
//! **approve** or **feedback** releases it with the decision, the note and
//! the review the finish filed; a review the reader leaves
//! is filed under the document's own scope and named in the answer, so the
//! agent reads it back like any review. A session spawned without the
//! `review` family still hands off, but its description and answer name the
//! family it lacks instead of telling it to call a tool it does not have.
//! `NotNow` ends the call with the reader's decline. A detached client
//! leaves the call parked, and the ask is re-readable when a client
//! returns; only the waiter's own drop abandons it.
//!
//! **Any-caller and not toggleable, like the browser**: every session may
//! hand a document out of its own tree, and the read resolves against that
//! tree or its project's `document_roots` - so there is nothing a project
//! could withhold. Every state the read can refuse is the call's failure,
//! with the read's own sentence in it.

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
    review_available: bool,
) -> forge_sdk::mcp::server::McpServer {
    add_tools(
        McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")),
        facade,
        slot,
        review_available,
    )
    .build()
}

/// Attach the hand-off tool to an existing builder, so it shares the `forge`
/// server name with every other family.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn DocumentFacade>,
    slot: SessionSlot,
    review_available: bool,
) -> McpServerBuilder {
    builder.tool(HandOffTool { facade, slot, review_available })
}

/// What the model reads about the tool before calling it: what it hands
/// over, when `reviewable` earns its place, and which moment this answers.
const DESCRIPTION: &str = "\
Hand the reader a document from this session's working tree, or from one of \
the project's document roots, opened in their own viewer. This tool is for \
documents - Markdown, PDF, plain text; a visual artifact - an HTML mockup, \
a PNG - goes to the browser instead: navigate to it and hand off that tab \
with `browser_hand_off`, never this tool. Use it when a \
document is the thing the reader should see - a plan, a report, a spec - \
rather than part of the conversation; a document the reader is meant to \
review goes here with `reviewable: true`, never as a path or link in \
chat. `path` is relative to the working directory, or absolute under \
one of the project's `document_roots` in `forge.toml`. Set `reviewable` \
when you want review comments back: the reader opens it, comments on \
lines and answers, and their review then arrives filed under the \
document's own scope. A PDF always opens read-only. **The answer is \
approve or send feedback**: feedback means revise the document, hand it \
off again, and keep handing it off until the reader approves; approval \
is the one signal to proceed, and neither may be read as the other; the \
one feedback box reaches you with either \
answer, though approve may leave it empty and send feedback may not. \
The reader is asked to open it or not, and **this call waits for them**: \
it answers when they approve, send feedback or say not now.";

/// What a session spawned without `review` reads instead, before it calls:
/// the hand-off works and the reader may still review, but the review is
/// the lead's to read, so the plain description would promise it back. The
/// paragraphs it shares with `DESCRIPTION` change in both.
const DESCRIPTION_WITHOUT_REVIEW: &str = "\
Hand the reader a document from this session's working tree, or from one of \
the project's document roots, opened in their own viewer. This tool is for \
documents - Markdown, PDF, plain text; a visual artifact - an HTML mockup, \
a PNG - goes to the browser instead: navigate to it and hand off that tab \
with `browser_hand_off`, never this tool. Use it when a \
document is the thing the reader should see - a plan, a report, a spec - \
rather than part of the conversation; a document the reader is meant to \
review goes here with `reviewable: true`, never as a path or link in \
chat. `path` is relative to the working directory, or absolute under \
one of the project's `document_roots` in `forge.toml`. Set `reviewable` \
to ask the reader for a review of it: they open it, comment on lines \
and answer, and their review is filed under the document's own scope. \
This session was spawned without the `review` MCP family, so you cannot \
read it back; the answer names the review's id and scope, which your lead \
reads with review__get. A PDF always opens read-only. **The answer is \
approve or send feedback**: feedback means revise the document, hand it \
off again, and keep handing it off until the reader approves; approval \
is the one signal to proceed, and neither may be read as the other; the \
one feedback box reaches you with either \
answer, though approve may leave it empty and send feedback may not. \
The reader is asked to open it or not, and **this call waits for them**: \
it answers when they approve, send feedback or say not now.";

/// The one tool: the hand-off itself.
pub(crate) struct HandOffTool {
    facade: Arc<dyn DocumentFacade>,
    slot: SessionSlot,
    /// Whether this session was built with the `review` family, which
    /// decides whether its answer can send it to `review__get`.
    review_available: bool,
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
        if self.review_available { DESCRIPTION } else { DESCRIPTION_WITHOUT_REVIEW }
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The document's path, relative to the session's working \
                                    directory, or absolute under one of the project's \
                                    `document_roots` in `forge.toml`.",
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
                 session's working directory or absolute under one of the project's document_roots"
                .to_owned());
        };
        let reviewable = match args.get("reviewable") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(other) => return Err(format!("`reviewable` is true or false, not {other}")),
        };
        let (resolved, ending) = self.facade.hand_off(&self.slot, path, reviewable).await?;
        Ok(match ending {
            DocumentHandOffEnding::Approve { review_id, note } => {
                let opened = format!(
                    "The reader approved {resolved}. Approval is the one signal to proceed."
                );
                format!(
                    "{opened}{}{}",
                    Self::note_line(note.as_deref()),
                    Self::review_line(self.review_available, &resolved, review_id.as_deref())
                )
            }
            DocumentHandOffEnding::Feedback { review_id, note } => {
                let refused = format!(
                    "The reader sent feedback at {resolved}. This is not approval: \
                     revise the document and hand it off again, and keep handing it \
                     off until the reader approves."
                );
                return Err(format!(
                    "{refused}{}{}",
                    Self::note_line(note.as_deref()),
                    Self::review_line(self.review_available, &resolved, review_id.as_deref())
                ));
            }
            DocumentHandOffEnding::NotNow => {
                return Err("the reader said not now, so nothing opened".to_owned());
            }
            DocumentHandOffEnding::Abandoned => {
                return Err("The hand-off ended without an answer.".to_owned());
            }
        })
    }

    /// The reader's own words, carried verbatim and said to be theirs.
    fn note_line(note: Option<&str>) -> String {
        match note {
            Some(note) => format!(" Their note: {note}"),
            None => String::new(),
        }
    }

    /// The review a finish filed, named the way this session can act on it:
    /// the id and scope plus the call that reads it back, or - for a
    /// session spawned without the family - the lead it hands the scope to.
    fn review_line(review_available: bool, resolved: &str, review_id: Option<&str>) -> String {
        let Some(review_id) = review_id else {
            return String::new();
        };
        let scope = doc_scope_ref(resolved);
        if review_available {
            format!(
                " Their comments are filed as the review {review_id} under the document's own \
                 scope, {scope} - read them back with review__get and pass that scope."
            )
        } else {
            format!(
                " Their comments are filed as the review {review_id} under the document's own \
                 scope, {scope}. This session was spawned without the `review` MCP family, so \
                 you cannot read them back - hand your lead the review id and scope above, \
                 which it reads with review__get."
            )
        }
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
        tool_with_review(facade, true)
    }

    fn tool_with_review(facade: Arc<dyn DocumentFacade>, review_available: bool) -> HandOffTool {
        HandOffTool { facade, slot: seat(), review_available }
    }

    /// The whole call: the path and the reviewable flag cross to the
    /// hand-off for the caller's own seat, and the answer carries the
    /// resolved path and the ending the park released on.
    #[tokio::test]
    async fn a_hand_off_carries_the_path_and_the_flag_for_the_callers_seat() {
        let mock = Arc::new(MockDocumentFacade::new());
        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Approve { review_id: None, note: None },
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
            DocumentHandOffEnding::Approve { review_id: Some("r-7".to_owned()), note: None },
        ));
        let tool = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>);

        let with_review = tool.call(ToolInput { value: json!({ "path": "docs/readme.md" }) }).await;
        assert!(!with_review.is_error, "{:?}", with_review.blocks);
        let text = crate::mcp::test_support::block_text(&with_review.blocks[0]);
        assert!(text.contains("r-7"), "the answer names the review: {text}");
        assert!(text.contains("doc:/proj/docs/readme.md"), "and the scope: {text}");
        assert!(
            text.contains("read them back with review__get and pass that scope"),
            "and the call that reads them back, for a session that has it: {text}",
        );

        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Approve { review_id: None, note: None },
        ));
        let without = tool.call(ToolInput { value: json!({ "path": "docs/readme.md" }) }).await;
        assert!(!without.is_error, "{:?}", without.blocks);
        let text = crate::mcp::test_support::block_text(&without.blocks[0]);
        assert!(!text.contains("doc:"), "nothing commented, so no scope to name: {text}");
    }

    /// **Approve and feedback are different answers, and the note rides
    /// both verbatim.** Approval is the one signal an agent may act on;
    /// feedback says the opposite in its own words and keeps the reader's
    /// note exactly as they wrote it.
    #[tokio::test]
    async fn approve_and_feedback_carry_the_note_and_never_read_as_each_other() {
        let mock = Arc::new(MockDocumentFacade::new());
        let tool = tool(Arc::clone(&mock) as Arc<dyn DocumentFacade>);

        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Approve {
                review_id: Some("r-7".to_owned()),
                note: Some("ship it".to_owned()),
            },
        ));
        let approved = tool
            .call(ToolInput { value: json!({ "path": "docs/readme.md", "reviewable": true }) })
            .await;
        assert!(!approved.is_error, "an approval is a success: {:?}", approved.blocks);
        let text = crate::mcp::test_support::block_text(&approved.blocks[0]);
        assert!(text.contains("approved"), "the answer says what happened: {text}");
        assert!(text.contains("ship it"), "and carries the note verbatim: {text}");
        assert!(text.contains("read them back with review__get"), "and the review: {text}");

        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Feedback {
                review_id: Some("r-8".to_owned()),
                note: Some("the table is wrong".to_owned()),
            },
        ));
        let refused = tool
            .call(ToolInput { value: json!({ "path": "docs/readme.md", "reviewable": true }) })
            .await;
        assert!(refused.is_error, "feedback is not a hand-off success: {:?}", refused.blocks);
        let text = crate::mcp::test_support::block_text(&refused.blocks[0]);
        assert!(text.contains("sent feedback"), "the reader's answer is named: {text}");
        assert!(text.contains("the table is wrong"), "the note reaches the agent: {text}");
        assert!(
            text.contains("revise the document and hand it off again"),
            "and the loop's next step is stated: {text}",
        );
        assert!(
            text.contains("keep handing it off until the reader approves"),
            "and the loop is until approval: {text}",
        );
        assert!(text.contains("not approval"), "never read as done: {text}");
    }

    /// A session spawned without the `review` family can hand a reviewable
    /// document off, but it cannot read the review back. Its answer names
    /// the family it lacks and routes the scope to its lead instead of
    /// sending it to a `review__get` it does not have.
    #[tokio::test]
    async fn a_hand_off_from_a_session_without_review_names_the_family_it_lacks() {
        let mock = Arc::new(MockDocumentFacade::new());
        let tool = tool_with_review(Arc::clone(&mock) as Arc<dyn DocumentFacade>, false);

        *mock.answer.lock() = Ok((
            "/proj/docs/readme.md".to_owned(),
            DocumentHandOffEnding::Approve { review_id: Some("r-7".to_owned()), note: None },
        ));
        let out = tool
            .call(ToolInput { value: json!({ "path": "docs/readme.md", "reviewable": true }) })
            .await;
        assert!(!out.is_error, "{:?}", out.blocks);
        let text = crate::mcp::test_support::block_text(&out.blocks[0]);
        assert!(text.contains("r-7"), "the review is still named: {text}");
        assert!(
            text.contains("spawned without the `review` MCP family"),
            "the answer names the family the session lacks: {text}",
        );
        assert!(text.contains("your lead"), "and routes the scope to the lead: {text}");
        assert!(
            !text.contains("read them back with review__get and pass that scope"),
            "it must not send a session without review to read them itself: {text}",
        );
    }

    /// The description is the surface read before the call, so a session
    /// spawned without `review` learns the review will not come back to it
    /// there, not only once the park releases.
    #[test]
    fn the_description_names_the_review_gap_before_the_call() {
        let without = tool_with_review(MockDocumentFacade::new().into_arc(), false);
        assert!(
            without.description().contains("spawned without the `review` MCP family"),
            "the pre-call text names the family this session lacks: {}",
            without.description(),
        );
        let with = tool(MockDocumentFacade::new().into_arc());
        assert!(
            !with.description().contains("spawned without the `review` MCP family"),
            "and a session that has it is not told otherwise: {}",
            with.description(),
        );
    }

    /// The reader's decline and an abandoned hand-off are the call's
    /// failures, each with its own sentence - the agent must not read either
    /// as done.
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
        assert_eq!(text, "The hand-off ended without an answer.", "{text}");
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

    /// The description is read at the moment a caller reaches for the tool,
    /// so the delivery rule lives there too: a document the reader is meant
    /// to review goes through the hand-off, never as a path or link in
    /// chat.
    #[test]
    fn the_description_sends_a_review_document_through_the_hand_off() {
        for review_available in [true, false] {
            let tool = tool_with_review(MockDocumentFacade::new().into_arc(), review_available);
            assert!(
                tool.description().contains(
                    "a document the reader is meant to review goes here with `reviewable: true`, \
                     never as a path or link in chat"
                ),
                "the description carries the delivery rule (review_available: {review_available}): \
                 {}",
                tool.description(),
            );
        }
    }

    /// **A visual artifact is not a document.** The description is read
    /// before the call, and an HTML mockup or a PNG pushed through it opens
    /// as a file the reader cannot use; both descriptions route those to the
    /// browser's tab hand-off instead.
    #[test]
    fn both_descriptions_route_visual_artifacts_to_the_browser() {
        for review_available in [true, false] {
            let tool = tool_with_review(MockDocumentFacade::new().into_arc(), review_available);
            let description = tool.description();
            for phrase in [
                "a visual artifact - an HTML mockup, a PNG - goes to the browser instead",
                "hand off that tab with `browser_hand_off`, never this tool",
            ] {
                assert!(
                    description.contains(phrase),
                    "the routing rule is stated (review_available: {review_available}): \
                     {description}",
                );
            }
        }
    }

    /// **The loop is stated before the call, in both descriptions.** A
    /// session reads the paragraph at the moment it reaches for the tool:
    /// the two outcomes, feedback's next step and approval's exclusivity,
    /// and the note that rides them. The conduct block pins the same
    /// contract for a session that never opens the tool.
    #[test]
    fn both_descriptions_state_the_hand_offs_loop() {
        for review_available in [true, false] {
            let tool = tool_with_review(MockDocumentFacade::new().into_arc(), review_available);
            let description = tool.description();
            assert!(
                description.contains("The answer is approve or send feedback"),
                "the two outcomes are named (review_available: {review_available}): {description}",
            );
            assert!(
                description.contains(
                    "feedback means revise the document, hand it off again, and keep handing it \
                     off until the reader approves; approval is the one signal to proceed, and \
                     neither may be read as the other"
                ),
                "feedback's next step and approval's exclusivity are stated \
                 (review_available: {review_available}): {description}",
            );
            assert!(
                description.contains(
                    "the one feedback box reaches you with either answer, though approve may \
                     leave it empty and send feedback may not"
                ),
                "the box's delivery is stated (review_available: {review_available}): {description}",
            );
        }
    }

    /// The family registers exactly one tool, under the name a prompt calls
    /// it by.
    #[test]
    fn the_family_registers_one_tool_named_documents_hand_off() {
        let server = build_server(MockDocumentFacade::new().into_arc(), seat(), true);
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
