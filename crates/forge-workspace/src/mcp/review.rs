//! Review-conversation tools (Phase 2).
//!
//! Worker-facing MCP tools that turn a submitted review set into a
//! two-way conversation. The reviewer submits comments via `/diff`; the
//! worker reads them, replies, and resolves through
//! `mcp__forge__review__*`, and each reply appends to the comment thread
//! and flips it `Open` -> `Addressed`.
//!
//! This module holds the JSON view shapes the tools return plus the pure
//! mappers from the durable [`forge_primitives::review`] types. The store
//! I/O lives in [`crate::store::review`]; the `Workspace::review_*`
//! wrappers surface it; the `Tool` impls + caller resolution live beside
//! this (the `facade` submodule and the tool structs).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[cfg(test)]
use forge_sdk::mcp::server::McpServer;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use forge_primitives::review::{ReviewAuthor, ReviewSet, ReviewSide, ReviewStatus, ReviewThread};

use crate::mcp::peers::facade::CallerKeyResolver;
use crate::mcp::review::facade::{ReviewFacade, ReviewScope};

pub mod facade;

/// One row of `review__list`: a submitted review plus its member-comment
/// tally by state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewSummary {
    pub review_id: String,
    pub number: u32,
    pub summary: Option<String>,
    pub created_at: String,
    pub comment_count: usize,
    pub open: usize,
    pub addressed: usize,
    pub resolved: usize,
    pub outdated: usize,
}

/// `review__get`: a review's overview plus the comments filed under it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewDetail {
    pub review_id: String,
    pub number: u32,
    pub summary: Option<String>,
    pub comments: Vec<ReviewCommentView>,
}

/// One comment in `review__get`, carrying the anchored code so the worker
/// can locate the spot regardless of the comment's diff scope (a
/// commit-scoped comment's line may not match the current working tree).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewCommentView {
    pub comment_id: String,
    pub file: String,
    pub line: u32,
    pub side: &'static str,
    pub status: &'static str,
    /// The captured lines around the anchored spot
    /// ([`forge_primitives::review::ReviewAnchor::context`]), so a shifted
    /// line number still locates the code the comment refers to.
    pub context: Vec<String>,
    pub thread: Vec<ReviewTurnView>,
}

/// One turn in a comment thread: `author` is `"you"` for the reviewer or
/// the agent's label for a worker reply. `review` is the number of the
/// review that sealed the turn, so a thread spanning several rounds shows
/// which turns belong to which; `None` on agent replies and on a turn no
/// review has sealed yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewTurnView {
    pub author: String,
    pub text: String,
    pub at: String,
    pub review: Option<u32>,
}

/// One worker review action recorded during a turn, accumulated per
/// caller and drained at the turn's end into a single notice per review.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReviewActivity {
    pub project: String,
    pub branch: String,
    pub review_id: String,
    /// `true` for a reply, `false` for a resolve.
    pub replied: bool,
}

/// The reviewer-facing one-line summary a worker's review turn produces:
/// `worker addressed review #N - A replied, B resolved, C open. Open /diff.`
/// `open` is the review's current open-comment count after the turn.
pub(crate) fn notice_message(number: u32, replied: usize, resolved: usize, open: usize) -> String {
    format!(
        "worker addressed review #{number} - {replied} replied, {resolved} resolved, \
         {open} open. Open /diff."
    )
}

fn side_str(side: ReviewSide) -> &'static str {
    match side {
        ReviewSide::Old => "old",
        ReviewSide::New => "new",
    }
}

fn status_str(status: ReviewStatus) -> &'static str {
    match status {
        ReviewStatus::Open => "open",
        ReviewStatus::Addressed => "addressed",
        ReviewStatus::Resolved => "resolved",
        ReviewStatus::Outdated => "outdated",
    }
}

fn author_str(author: &ReviewAuthor) -> String {
    match author {
        ReviewAuthor::User => "you".to_owned(),
        ReviewAuthor::Agent { label } => label.clone(),
    }
}

/// Build the `review__list` rows for `reviews` given every thread on the
/// branch, newest review first. A review's members are the threads with a
/// turn filed into it, so a thread the reviewer replied on across rounds
/// counts under each of them.
pub(crate) fn summarize(reviews: &[ReviewSet], threads: &[ReviewThread]) -> Vec<ReviewSummary> {
    reviews
        .iter()
        .rev()
        .map(|review| {
            let (mut open, mut addressed, mut resolved, mut outdated) = (0, 0, 0, 0);
            let mut count = 0;
            for t in threads.iter().filter(|t| t.is_in_review(&review.id)) {
                count += 1;
                match t.status {
                    ReviewStatus::Open => open += 1,
                    ReviewStatus::Addressed => addressed += 1,
                    ReviewStatus::Resolved => resolved += 1,
                    ReviewStatus::Outdated => outdated += 1,
                }
            }
            ReviewSummary {
                review_id: review.id.clone(),
                number: review.number,
                summary: review.summary.clone(),
                created_at: review.created_at.clone(),
                comment_count: count,
                open,
                addressed,
                resolved,
                outdated,
            }
        })
        .collect()
}

/// Build the `review__get` detail for `review_id`, or `None` when no such
/// review exists on the branch.
pub(crate) fn detail(
    reviews: &[ReviewSet],
    threads: &[ReviewThread],
    review_id: &str,
) -> Option<ReviewDetail> {
    let review = reviews.iter().find(|r| r.id == review_id)?;
    let comments = threads
        .iter()
        .filter(|t| t.is_in_review(review_id))
        .map(|t| comment_view(t, reviews))
        .collect();
    Some(ReviewDetail {
        review_id: review.id.clone(),
        number: review.number,
        summary: review.summary.clone(),
        comments,
    })
}

/// Map one thread to its `review__get` view. The whole conversation is
/// carried, not just the requested review's turns, so the worker reads a
/// later round with the earlier exchange as context; each turn's `review`
/// says which round it came from.
fn comment_view(thread: &ReviewThread, reviews: &[ReviewSet]) -> ReviewCommentView {
    ReviewCommentView {
        comment_id: thread.id.clone(),
        file: thread.anchor.path.clone(),
        line: thread.anchor.line,
        side: side_str(thread.anchor.side),
        status: status_str(thread.status),
        context: thread.anchor.context.clone(),
        thread: thread
            .comments
            .iter()
            .map(|c| ReviewTurnView {
                author: author_str(&c.author),
                text: c.text.clone(),
                at: c.at.clone(),
                review: c
                    .review_id
                    .as_deref()
                    .and_then(|id| reviews.iter().find(|r| r.id == id).map(|r| r.number)),
            })
            .collect(),
    }
}

/// Build a standalone `forge` MCP server carrying only the four
/// review-conversation tools. Test-only; production shares one `forge`
/// server via [`crate::mcp::build_forge_server`].
#[cfg(test)]
pub fn build_server(facade: Arc<dyn ReviewFacade>, caller_key: CallerKeyResolver) -> McpServer {
    add_tools(McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")), facade, caller_key).build()
}

/// Attach the four review-conversation tools to an existing builder. The
/// parent module's `build_forge_server` calls this so they share the
/// `forge` server name with the peers / workers tools.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn ReviewFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    builder
        .tool(ReviewList { facade: facade.clone(), caller_key: caller_key.clone() })
        .tool(ReviewGet { facade: facade.clone(), caller_key: caller_key.clone() })
        .tool(ReviewReply { facade: facade.clone(), caller_key: caller_key.clone() })
        .tool(ReviewResolve { facade, caller_key })
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

fn json_or_error<T: Serialize>(value: &T) -> ToolOutput {
    match serde_json::to_string_pretty(value) {
        Ok(json) => ToolOutput::text(json),
        Err(err) => tool_error(format!("response serialization failed: {err}")),
    }
}

/// RFC3339 timestamp for the current instant, matching the peers helper.
fn rfc3339_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "rfc3339 format failed; emitting epoch sentinel");
        "1970-01-01T00:00:00Z".to_owned()
    })
}

/// Resolve the caller's review scope or a ready-to-return tool error
/// naming the step that failed (see [`ScopeError`]).
async fn scope_or_error(
    facade: &Arc<dyn ReviewFacade>,
    caller_key: &CallerKeyResolver,
) -> Result<ReviewScope, ToolOutput> {
    let caller = caller_key.current().map_err(|err| tool_error(err.to_string()))?;
    facade.resolve_scope(&caller).await.map_err(|err| tool_error(err.message()))
}

/// `review__list` - the submitted reviews on the caller's (project,
/// branch). No args.
pub(crate) struct ReviewList {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[async_trait::async_trait]
impl Tool for ReviewList {
    fn name(&self) -> &'static str {
        "review__list"
    }

    fn description(&self) -> &'static str {
        "List the review sets the reviewer submitted on your current branch, \
         newest first. Each entry has a review_id, its 1-based number, the \
         optional overview summary, when it was created, and a per-state \
         tally of its comments (open / addressed / resolved / outdated). \
         Start here when you get a 'review #N ready' nudge, then call \
         review__get with a review_id to read the individual comments. \
         Takes no arguments."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let scope = match scope_or_error(&self.facade, &self.caller_key).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.list(&scope) {
            Ok(rows) => list_output(&self.facade, &scope, &rows),
            Err(err) => tool_error(err),
        }
    }
}

/// The caller's rows, plus the project's other review-bearing branches
/// when there are any. A branch-scoped list is indistinguishable from the
/// project's whole set, and a review on the caller's own branch is no
/// reason to leave the one beside it unseen.
fn list_output(
    facade: &Arc<dyn ReviewFacade>,
    scope: &ReviewScope,
    rows: &[ReviewSummary],
) -> ToolOutput {
    let others: Vec<String> = facade
        .branches_with_reviews(scope)
        .into_iter()
        .filter(|branch| *branch != scope.branch)
        .collect();
    if others.is_empty() {
        return json_or_error(&rows);
    }
    if rows.is_empty() {
        return tool_error(format!(
            "no reviews on {}; this project has reviews on: {}.",
            scope.branch,
            others.join(", "),
        ));
    }
    let mut out = json_or_error(&rows);
    if out.is_error {
        return out;
    }
    out.blocks.push(ToolOutputBlock {
        text: format!("this project also has reviews on: {}.", others.join(", ")),
    });
    out
}

/// `review__get` - the comments of one review, with anchored code.
pub(crate) struct ReviewGet {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(Deserialize)]
struct GetArgs {
    review_id: String,
}

#[async_trait::async_trait]
impl Tool for ReviewGet {
    fn name(&self) -> &'static str {
        "review__get"
    }

    fn description(&self) -> &'static str {
        "Read one review's overview and its comments. Returns the review's \
         summary plus a comment array; each comment has a comment_id (use \
         it with review__reply / review__resolve), its file, line, and \
         side, its current status, the captured `context` lines of code \
         around the anchored spot, and the thread of turns so far (author \
         'you' is the reviewer, otherwise a worker reply). Prefer the \
         `context` code snippet over the line number to locate the spot - \
         a comment may be scoped to a commit whose line no longer matches \
         your working tree. Pass the review_id from review__list."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "review_id": {
                    "type": "string",
                    "description": "The review_id from review__list.",
                },
            },
            "required": ["review_id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: GetArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let scope = match scope_or_error(&self.facade, &self.caller_key).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.get(&scope, &args.review_id) {
            Ok(Some(detail)) => json_or_error(&detail),
            Ok(None) => tool_error(format!(
                "no review {} on your branch - call review__list for the valid ids.",
                args.review_id,
            )),
            Err(err) => tool_error(err),
        }
    }
}

/// `review__reply` - append a reply to a comment thread; flips it
/// Open -> Addressed.
pub(crate) struct ReviewReply {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(Deserialize)]
struct ReplyArgs {
    comment_id: String,
    text: String,
}

#[async_trait::async_trait]
impl Tool for ReviewReply {
    fn name(&self) -> &'static str {
        "review__reply"
    }

    fn description(&self) -> &'static str {
        "Reply to one review comment. Appends your message to that comment's \
         thread and flips it from open to addressed so the reviewer sees you \
         acted on it (a comment already resolved stays resolved). Use this to \
         say what you changed, ask a clarifying question, or push back. Pass \
         the comment_id from review__get. Returns the comment's new status. \
         Reply per comment, then use review__resolve for the ones you \
         consider done, or leave them addressed for the reviewer to resolve."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "comment_id": {
                    "type": "string",
                    "description": "The comment_id from review__get.",
                },
                "text": {
                    "type": "string",
                    "description": "Your reply, addressed to the reviewer.",
                },
            },
            "required": ["comment_id", "text"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ReplyArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let scope = match scope_or_error(&self.facade, &self.caller_key).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.reply(&scope, &args.comment_id, &args.text, &rfc3339_now()) {
            Ok(status) => json_or_error(&serde_json::json!({
                "comment_id": args.comment_id,
                "status": status_str(status),
            })),
            Err(err) => tool_error(err),
        }
    }
}

/// `review__resolve` - mark a comment resolved.
pub(crate) struct ReviewResolve {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(Deserialize)]
struct ResolveArgs {
    comment_id: String,
}

#[async_trait::async_trait]
impl Tool for ReviewResolve {
    fn name(&self) -> &'static str {
        "review__resolve"
    }

    fn description(&self) -> &'static str {
        "Mark one review comment resolved - you consider it done. Prefer \
         replying first (review__reply) to say what you changed, then \
         resolve; or leave a comment addressed and let the reviewer resolve \
         it themselves. Pass the comment_id from review__get."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "comment_id": {
                    "type": "string",
                    "description": "The comment_id from review__get.",
                },
            },
            "required": ["comment_id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ResolveArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let scope = match scope_or_error(&self.facade, &self.caller_key).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.resolve(&scope, &args.comment_id) {
            Ok(()) => json_or_error(&serde_json::json!({
                "comment_id": args.comment_id,
                "status": "resolved",
            })),
            Err(err) => tool_error(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::review::{ReviewAnchor, ReviewComment};

    fn review(id: &str, number: u32, summary: Option<&str>) -> ReviewSet {
        ReviewSet {
            id: id.to_owned(),
            number,
            summary: summary.map(str::to_owned),
            created_at: "2026-07-23T10:00:00Z".to_owned(),
        }
    }

    fn thread(id: &str, review_id: Option<&str>, status: ReviewStatus) -> ReviewThread {
        ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 42,
                content_hash: 7,
                context: vec!["fn f() {".to_owned(), "    todo!()".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("look at {id}"),
                at: "2026-07-23T10:00:00Z".to_owned(),
                review_id: review_id.map(str::to_owned),
            }],
            status,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
        }
    }

    #[test]
    fn summarize_tallies_members_newest_first() {
        let reviews = vec![review("r1", 1, Some("first")), review("r2", 2, None)];
        let threads = vec![
            thread("a", Some("r1"), ReviewStatus::Open),
            thread("b", Some("r1"), ReviewStatus::Addressed),
            thread("c", Some("r1"), ReviewStatus::Resolved),
            thread("d", Some("r2"), ReviewStatus::Outdated),
            thread("e", None, ReviewStatus::Open),
        ];
        let rows = summarize(&reviews, &threads);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].review_id, "r2", "newest review leads");
        assert_eq!(rows[0].comment_count, 1);
        assert_eq!(rows[0].outdated, 1);
        assert_eq!(rows[1].review_id, "r1");
        assert_eq!(rows[1].comment_count, 3, "the unfiled thread does not tally into r1");
        assert_eq!(rows[1].open, 1);
        assert_eq!(rows[1].addressed, 1);
        assert_eq!(rows[1].resolved, 1);
        assert_eq!(rows[1].summary.as_deref(), Some("first"));
    }

    #[test]
    fn detail_maps_comments_with_anchor_context() {
        let reviews = vec![review("r1", 1, Some("overview"))];
        let mut t = thread("a", Some("r1"), ReviewStatus::Addressed);
        t.anchor.side = ReviewSide::Old;
        t.comments.push(ReviewComment {
            author: ReviewAuthor::Agent { label: "implementer".to_owned() },
            text: "done".to_owned(),
            at: "2026-07-23T11:00:00Z".to_owned(),
            review_id: None,
        });
        let got = detail(&reviews, &[t], "r1").expect("review found");
        assert_eq!(got.number, 1);
        assert_eq!(got.summary.as_deref(), Some("overview"));
        assert_eq!(got.comments.len(), 1);
        let c = &got.comments[0];
        assert_eq!(c.comment_id, "a");
        assert_eq!(c.file, "src/x.rs");
        assert_eq!(c.line, 42);
        assert_eq!(c.side, "old");
        assert_eq!(c.status, "addressed");
        assert_eq!(c.context, vec!["fn f() {".to_owned(), "    todo!()".to_owned()]);
        assert_eq!(c.thread.len(), 2, "user comment + agent reply");
        assert_eq!(c.thread[0].author, "you");
        assert_eq!(c.thread[1].author, "implementer");
        assert_eq!(c.thread[1].text, "done");
    }

    /// A thread the reviewer replied on after the agent answered belongs to
    /// both rounds, and each round's view says which turn is new.
    #[test]
    fn detail_shows_a_multi_round_thread_under_every_review() {
        let reviews = vec![review("r1", 1, None), review("r2", 2, Some("round two"))];
        let mut t = thread("a", Some("r1"), ReviewStatus::Open);
        t.comments.push(ReviewComment {
            author: ReviewAuthor::Agent { label: "implementer".to_owned() },
            text: "done".to_owned(),
            at: "2026-07-23T11:00:00Z".to_owned(),
            review_id: None,
        });
        t.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "still not right".to_owned(),
            at: "2026-07-23T12:00:00Z".to_owned(),
            review_id: Some("r2".to_owned()),
        });
        let threads = [t];

        for (id, number) in [("r1", 1), ("r2", 2)] {
            let got = detail(&reviews, &threads, id).expect("review found");
            assert_eq!(got.number, number);
            assert_eq!(got.comments.len(), 1, "the thread is a member of {id}");
            assert_eq!(
                got.comments[0].thread.iter().map(|t| t.review).collect::<Vec<_>>(),
                vec![Some(1), None, Some(2)],
                "the whole conversation is carried, tagged by round",
            );
        }

        let rows = summarize(&reviews, &threads);
        assert_eq!(rows[0].comment_count, 1, "r2 counts it");
        assert_eq!(rows[1].comment_count, 1, "and so does r1");
    }

    #[test]
    fn detail_is_none_for_unknown_review() {
        let reviews = vec![review("r1", 1, None)];
        assert!(detail(&reviews, &[], "nope").is_none());
    }

    #[test]
    fn notice_message_reads_as_a_tally_line() {
        assert_eq!(
            notice_message(3, 2, 1, 4),
            "worker addressed review #3 - 2 replied, 1 resolved, 4 open. Open /diff.",
        );
    }

    use crate::SessionKey;
    use crate::mcp::review::facade::{MockReviewFacade, ScopeError};

    fn resolver() -> CallerKeyResolver {
        CallerKeyResolver::from_fixed(SessionKey::from_session_id("caller"))
    }

    fn summary(review_id: &str, number: u32) -> ReviewSummary {
        ReviewSummary {
            review_id: review_id.to_owned(),
            number,
            summary: Some("overview".to_owned()),
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            comment_count: 2,
            open: 1,
            addressed: 1,
            resolved: 0,
            outdated: 0,
        }
    }

    #[tokio::test]
    async fn review_list_returns_summaries_as_json() {
        let mock = Arc::new(MockReviewFacade::new());
        mock.summaries.lock().push(summary("r1", 1));
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "list happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&out.blocks[0].text).expect("json");
        assert_eq!(parsed[0]["review_id"], "r1");
        assert_eq!(parsed[0]["addressed"], 1);
    }

    #[tokio::test]
    async fn review_list_unresolved_scope_surfaces_the_failing_step() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.scope.lock() = Err(ScopeError::SessionCwdUnknown);
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(out.is_error, "an unresolved scope must surface as an error");
        assert_eq!(out.blocks[0].text, ScopeError::SessionCwdUnknown.message());
        assert!(
            !out.blocks[0].text.contains("detached"),
            "a non-HEAD failure must not claim a detached HEAD: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn review_list_empty_names_the_projects_other_branches() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.review_branches.lock() =
            vec!["feat".to_owned(), "main".to_owned(), "worktree-impl".to_owned()];
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(out.is_error, "a review filed against another branch must not read as 'none'");
        let text = &out.blocks[0].text;
        assert!(text.contains("no reviews on feat"), "{text}");
        assert!(text.contains("main") && text.contains("worktree-impl"), "{text}");
        assert!(!text.contains(": feat"), "the caller's own branch is not listed back: {text}");
    }

    #[tokio::test]
    async fn review_list_names_other_branches_alongside_a_non_empty_list() {
        // The cross-branch split this hint exists to catch is exactly
        // the case where the caller's own branch has a review too - a
        // lead reading its own review while the worker's sits beside it.
        let mock = Arc::new(MockReviewFacade::new());
        mock.summaries.lock().push(summary("r1", 1));
        *mock.review_branches.lock() = vec!["feat".to_owned(), "worktree-impl".to_owned()];
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "a populated list is not an error: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&out.blocks[0].text).expect("json");
        assert_eq!(parsed[0]["review_id"], "r1", "the rows stay the first block, still json");
        let hint = out.blocks.get(1).map_or("", |b| b.text.as_str());
        assert!(hint.contains("worktree-impl"), "{hint}");
        assert!(!hint.contains("feat"), "the caller's own branch is not listed back: {hint}");
    }

    #[tokio::test]
    async fn review_list_non_empty_says_nothing_when_no_other_branch_has_reviews() {
        let mock = Arc::new(MockReviewFacade::new());
        mock.summaries.lock().push(summary("r1", 1));
        *mock.review_branches.lock() = vec!["feat".to_owned()];
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert_eq!(out.blocks.len(), 1, "a normal list stays one json block: {:?}", out.blocks);
    }

    #[tokio::test]
    async fn review_list_empty_stays_an_empty_list_with_nothing_elsewhere() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "a project with no reviews anywhere is not an error");
        let parsed: serde_json::Value = serde_json::from_str(&out.blocks[0].text).expect("json");
        assert_eq!(parsed, serde_json::json!([]));
    }

    #[tokio::test]
    async fn review_get_returns_detail_then_not_found() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.detail.lock() = Some(ReviewDetail {
            review_id: "r1".to_owned(),
            number: 1,
            summary: Some("overview".to_owned()),
            comments: Vec::new(),
        });
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewGet { facade, caller_key: resolver() };
        let hit = tool.call(ToolInput { value: serde_json::json!({ "review_id": "r1" }) }).await;
        assert!(!hit.is_error);
        let parsed: serde_json::Value = serde_json::from_str(&hit.blocks[0].text).expect("json");
        assert_eq!(parsed["review_id"], "r1");
        let miss = tool.call(ToolInput { value: serde_json::json!({ "review_id": "r9" }) }).await;
        assert!(miss.is_error, "an unknown review_id is an error");
    }

    #[tokio::test]
    async fn review_reply_captures_and_returns_status() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, caller_key: resolver() };
        let out = tool
            .call(ToolInput {
                value: serde_json::json!({ "comment_id": "c1", "text": "fixed it" }),
            })
            .await;
        assert!(!out.is_error, "reply happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&out.blocks[0].text).expect("json");
        assert_eq!(parsed["status"], "addressed");
        assert_eq!(parsed["comment_id"], "c1");
        let calls = mock.reply_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], ("c1".to_owned(), "fixed it".to_owned()));
    }

    #[tokio::test]
    async fn review_reply_scope_rejection_surfaces_error() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.force_error.lock() = Some("no review comment c1 on (forge, feat)".to_owned());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, caller_key: resolver() };
        let out = tool
            .call(ToolInput { value: serde_json::json!({ "comment_id": "c1", "text": "x" }) })
            .await;
        assert!(out.is_error, "a cross-scope / unknown comment id must error");
        assert!(out.blocks[0].text.contains("no review comment"));
        assert_eq!(mock.reply_calls.lock().len(), 0, "a rejected reply captures no call");
    }

    #[tokio::test]
    async fn review_resolve_captures_comment_id() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewResolve { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c2" }) }).await;
        assert!(!out.is_error, "resolve happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&out.blocks[0].text).expect("json");
        assert_eq!(parsed["status"], "resolved");
        assert_eq!(*mock.resolve_calls.lock(), vec!["c2".to_owned()]);
    }

    #[tokio::test]
    async fn review_reply_invalid_args_is_error() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewReply { facade, caller_key: resolver() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c1" }) }).await;
        assert!(out.is_error, "missing 'text' is an error");
        assert!(out.blocks[0].text.to_lowercase().contains("invalid"));
    }

    #[test]
    fn build_server_registers_all_four_tools() {
        let facade = MockReviewFacade::new().into_arc();
        let server = build_server(facade, resolver());
        let debug = format!("{server:?}");
        for expected in ["review__list", "review__get", "review__reply", "review__resolve"] {
            assert!(debug.contains(expected), "build_server must include {expected}; {debug}");
        }
    }
}
