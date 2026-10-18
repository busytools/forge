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

use forge_primitives::review::{
    ReviewAuthor, ReviewSet, ReviewSide, ReviewStatus, ReviewThread, is_doc_scope, is_tree_scope,
    tree_scope_ref,
};

use crate::SessionSlot;
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

/// What `review__reply` and `review__resolve` answer with: where the
/// comment is anchored and the review round its latest turn belongs to,
/// alongside the state it now holds. `number` is the review's 1-based
/// ordinal, null while the thread is filed in no round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommentAnchorView {
    pub comment_id: String,
    pub file: String,
    pub line: u32,
    pub side: &'static str,
    pub status: &'static str,
    pub number: Option<u32>,
}

/// Build the anchor echo for `thread`, whose state just became `status`.
pub(crate) fn comment_anchor(
    thread: &ReviewThread,
    reviews: &[ReviewSet],
    status: ReviewStatus,
) -> CommentAnchorView {
    CommentAnchorView {
        comment_id: thread.id.clone(),
        file: thread.anchor.path.clone(),
        line: thread.anchor.line,
        side: side_str(thread.anchor.side),
        status: status_str(status),
        number: thread
            .latest_review()
            .and_then(|id| reviews.iter().find(|r| r.id == id).map(|r| r.number)),
    }
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
/// `open` is the review's current open-comment count after the turn, and
/// `scope` names the review itself, because **each scope's comments open in
/// a door of their own**: the document viewer, the files page, or the diff
/// overlay (the lead's gate, 2026-10-17: a tree scope pointed at `/diff`,
/// which never carries one - the same hole the doc scope's door had closed).
pub(crate) fn notice_message(
    number: u32,
    replied: usize,
    resolved: usize,
    open: usize,
    scope: &str,
) -> String {
    let door = if forge_primitives::review::is_doc_scope(scope) {
        "Open the document."
    } else if forge_primitives::review::is_tree_scope(scope) {
        "Open the files page."
    } else {
        "Open /diff."
    };
    format!(
        "worker addressed review #{number} - {replied} replied, {resolved} resolved, \
         {open} open. {door}"
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
pub fn build_server(facade: Arc<dyn ReviewFacade>, slot: SessionSlot) -> McpServer {
    add_tools(McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")), facade, slot).build()
}

/// Attach the four review-conversation tools to an existing builder. The
/// parent module's `build_forge_server` calls this so they share the
/// `forge` server name with the agents tools.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn ReviewFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    builder
        .tool(ReviewList { facade: facade.clone(), slot: slot.clone() })
        .tool(ReviewGet { facade: facade.clone(), slot: slot.clone() })
        .tool(ReviewReply { facade: facade.clone(), slot: slot.clone() })
        .tool(ReviewResolve { facade, slot })
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput::error(text)
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
/// naming the step that failed (see [`ScopeError`](facade::ScopeError)).
async fn scope_or_error(
    facade: &Arc<dyn ReviewFacade>,
    slot: &SessionSlot,
    scope: Option<&str>,
) -> Result<ReviewScope, ToolOutput> {
    facade.resolve_scope(slot, scope).await.map_err(|err| tool_error(err.message()))
}

/// The caller's seat-tree scope, derived from its resolved branch scope:
/// project, author label and caller carry over; the key is the seat's own
/// tree (a worker's worktree, the lead's checkout), which is where the
/// files page's reviews live. The tools consult it beside the branch on
/// their default (no `scope` argument) path.
fn seat_tree_scope(scope: &ReviewScope) -> ReviewScope {
    ReviewScope {
        project: scope.project.clone(),
        branch: tree_scope_ref(&scope.caller),
        author_label: scope.author_label.clone(),
        caller: scope.caller.clone(),
    }
}

/// Branch rows and seat-tree rows as one list, newest first. Review ids are
/// uuids, so none collide across the two scopes; `number` is per-scope and
/// CAN repeat, which is why the tree's rows come with the scope note
/// beside them.
fn merge_newest(
    branch_rows: Vec<ReviewSummary>,
    tree_rows: Vec<ReviewSummary>,
) -> Vec<ReviewSummary> {
    let mut rows = branch_rows;
    rows.extend(tree_rows);
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    rows
}

/// `review__list` - the submitted reviews on the caller's (project,
/// branch), or on an explicit `scope`.
pub(crate) struct ReviewList {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(Deserialize)]
struct ListArgs {
    scope: Option<String>,
}

#[async_trait::async_trait]
impl Tool for ReviewList {
    fn name(&self) -> &'static str {
        "review__list"
    }

    fn description(&self) -> &'static str {
        "List the review sets the reviewer submitted on your current branch, \
         newest first - plus any filed against your seat's own file tree \
         (the files page's comments). Each entry has a review_id, its 1-based \
         number, the optional overview summary, when it was created, and a \
         per-state tally of its comments (open / addressed / resolved / \
         outdated). Start here when you get a 'review #N ready' nudge, then \
         call review__get with a review_id to read the individual comments. \
         Pass `scope` to address a review filed elsewhere: one left on a \
         document hand-off is filed under the document's own scope, \
         doc:<resolved path>, which the hand-off answer names."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "description": "Address a scope other than your branch; a document's \
                                    review is filed under doc:<resolved path>, which the \
                                    hand-off that opened it names.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ListArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let scope = match scope_or_error(&self.facade, &self.slot, args.scope.as_deref()).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.list(&scope) {
            Ok(rows) => {
                // The seat's tree rides beside the branch (files-page
                // reviews follow the seat), unless an explicit scope asked
                // for one scope alone. A tree read that failed never folds
                // into an empty answer: the branch rows still return, with
                // the failure stated beside them.
                let mut note = None;
                let rows = match args.scope.is_none().then(|| seat_tree_scope(&scope)) {
                    Some(tree) => match self.facade.list(&tree) {
                        Ok(tree_rows) => {
                            if !tree_rows.is_empty() {
                                note = Some(format!(
                                    "your seat tree's reviews are included (scope {}).",
                                    tree.branch,
                                ));
                            }
                            merge_newest(rows, tree_rows)
                        }
                        Err(err) => {
                            note =
                                Some(format!("your seat tree's reviews could not be read: {err}"));
                            rows
                        }
                    },
                    None => rows,
                };
                let mut out = list_output(&self.facade, &scope, &rows);
                if !out.is_error
                    && let Some(note) = note
                {
                    out.blocks.push(ToolOutputBlock::Text { text: note });
                }
                out
            }
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
        // Scope keys stored in the branch column are not branches. A tree
        // scope (the seat's own) or a document's would read as something
        // to check out; the tree's reviews already ride the list above,
        // and a document's are addressed through its hand-off.
        .filter(|branch| {
            *branch != scope.branch && !is_tree_scope(branch) && !is_doc_scope(branch)
        })
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
    out.blocks.push(ToolOutputBlock::Text {
        text: format!("this project also has reviews on: {}.", others.join(", ")),
    });
    out
}

/// `review__get` - the comments of one review, with anchored code.
pub(crate) struct ReviewGet {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(Deserialize)]
struct GetArgs {
    review_id: String,
    scope: Option<String>,
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
         your working tree. Pass the review_id from review__list, plus \
         `scope` for a review filed under a document's scope rather than \
         your branch."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "review_id": {
                    "type": "string",
                    "description": "The review_id from review__list.",
                },
                "scope": {
                    "type": "string",
                    "description": "A scope other than your branch; a document's review is \
                                    filed under doc:<resolved path>, which the hand-off \
                                    that opened it names.",
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
        let scope = match scope_or_error(&self.facade, &self.slot, args.scope.as_deref()).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.get(&scope, &args.review_id) {
            Ok(Some(detail)) => json_or_error(&detail),
            Ok(None) => {
                // An id the branch does not hold may live on the seat's
                // tree (the files page); the fallback runs only on the
                // default path, so an explicit scope still means that
                // scope alone.
                match args.scope.is_none().then(|| seat_tree_scope(&scope)) {
                    Some(tree) => match self.facade.get(&tree, &args.review_id) {
                        Ok(Some(detail)) => json_or_error(&detail),
                        Ok(None) => tool_error(format!(
                            "no review {} on your branch or your seat tree - call \
                             review__list for the valid ids.",
                            args.review_id,
                        )),
                        Err(err) => tool_error(err),
                    },
                    None => tool_error(format!(
                        "no review {} on your branch - call review__list for the valid ids.",
                        args.review_id,
                    )),
                }
            }
            Err(err) => tool_error(err),
        }
    }
}

/// `review__reply` - append a reply to a comment thread; flips it
/// Open -> Addressed.
pub(crate) struct ReviewReply {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(Deserialize)]
struct ReplyArgs {
    comment_id: String,
    text: String,
    scope: Option<String>,
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
         the comment_id from review__get. Returns the comment's new status \
         alongside the file, line and side it is anchored to. \
         Reply per comment, then use review__resolve for the ones you \
         consider done, or leave them addressed for the reviewer to resolve. \
         For a review filed under a document's scope rather than your \
         branch, pass `scope`."
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
                "scope": {
                    "type": "string",
                    "description": "A scope other than your branch; a document's review is \
                                    filed under doc:<resolved path>, which the hand-off \
                                    that opened it names.",
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
        let scope = match scope_or_error(&self.facade, &self.slot, args.scope.as_deref()).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.reply(&scope, &args.comment_id, &args.text, &rfc3339_now()) {
            Ok(anchor) => json_or_error(&anchor),
            Err(err) => {
                // A comment the branch's scope does not hold may live on
                // the seat's tree (the files page); the branch's own error
                // is what returns when the tree does not hold it either.
                if let Some(tree) = args.scope.is_none().then(|| seat_tree_scope(&scope))
                    && let Ok(anchor) =
                        self.facade.reply(&tree, &args.comment_id, &args.text, &rfc3339_now())
                {
                    return json_or_error(&anchor);
                }
                tool_error(err)
            }
        }
    }
}

/// `review__resolve` - mark a comment resolved.
pub(crate) struct ReviewResolve {
    pub(crate) facade: Arc<dyn ReviewFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(Deserialize)]
struct ResolveArgs {
    comment_id: String,
    scope: Option<String>,
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
         it themselves. Pass the comment_id from review__get. Returns the \
         comment's new status alongside the file, line and side it is \
         anchored to. For a review filed under a document's scope rather \
         than your branch, pass `scope`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "comment_id": {
                    "type": "string",
                    "description": "The comment_id from review__get.",
                },
                "scope": {
                    "type": "string",
                    "description": "A scope other than your branch; a document's review is \
                                    filed under doc:<resolved path>, which the hand-off \
                                    that opened it names.",
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
        let scope = match scope_or_error(&self.facade, &self.slot, args.scope.as_deref()).await {
            Ok(s) => s,
            Err(out) => return out,
        };
        match self.facade.resolve(&scope, &args.comment_id) {
            Ok(anchor) => json_or_error(&anchor),
            Err(err) => {
                // The seat-tree fallback, as in review__reply: the
                // branch's own error returns when the tree does not hold
                // the comment either.
                if let Some(tree) = args.scope.is_none().then(|| seat_tree_scope(&scope))
                    && let Ok(anchor) = self.facade.resolve(&tree, &args.comment_id)
                {
                    return json_or_error(&anchor);
                }
                tool_error(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::test_support::{block_text, text_of};
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
            notice_message(3, 2, 1, 4, "feat"),
            "worker addressed review #3 - 2 replied, 1 resolved, 4 open. Open /diff.",
        );
    }

    /// A document's review names the viewer as its door; `/diff` carries no
    /// doc scope, so pointing there would send the reader nowhere.
    #[test]
    fn notice_message_for_a_document_names_the_viewer() {
        let message = notice_message(1, 1, 0, 1, "doc:/proj/plan.md");
        assert!(message.contains("Open the document."), "{message}");
        assert!(!message.contains("/diff"), "{message}");
    }

    /// **A seat's tree has its own door too** (the lead's gate, 2026-10-17):
    /// the files page carries the tree scope, and `/diff` never does.
    #[test]
    fn notice_message_for_a_seat_tree_names_the_files_page() {
        let message = notice_message(1, 1, 0, 1, "tree:Busytools/forge/lead");
        assert!(message.contains("Open the files page."), "{message}");
        assert!(!message.contains("/diff"), "{message}");
    }

    use crate::mcp::review::facade::{MockReviewFacade, ProdReviewFacade, ScopeError};

    fn caller_slot() -> SessionSlot {
        SessionSlot::from_str_for_test("caller")
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
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "list happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed[0]["review_id"], "r1");
        assert_eq!(parsed[0]["addressed"], 1);
    }

    #[tokio::test]
    async fn review_list_unresolved_scope_surfaces_the_failing_step() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.scope.lock() = Err(ScopeError::SessionCwdUnknown);
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(out.is_error, "an unresolved scope must surface as an error");
        assert_eq!(text_of(&out), ScopeError::SessionCwdUnknown.message());
        assert!(
            !text_of(&out).contains("detached"),
            "a non-HEAD failure must not claim a detached HEAD: {}",
            text_of(&out),
        );
    }

    #[tokio::test]
    async fn review_list_empty_names_the_projects_other_branches() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.review_branches.lock() =
            vec!["feat".to_owned(), "main".to_owned(), "worktree-impl".to_owned()];
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(out.is_error, "a review filed against another branch must not read as 'none'");
        let text = text_of(&out);
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
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "a populated list is not an error: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed[0]["review_id"], "r1", "the rows stay the first block, still json");
        let hint = out.blocks.get(1).map_or("", block_text);
        assert!(hint.contains("worktree-impl"), "{hint}");
        assert!(!hint.contains("feat"), "the caller's own branch is not listed back: {hint}");
    }

    /// The seat's tree rides the list beside the branch: files-page
    /// reviews follow the seat, so one call returns both - the branch
    /// scope first, then the tree - with the tree's scope named so its
    /// rows can be told apart (`number` repeats across scopes; ids do
    /// not).
    #[tokio::test]
    async fn review_list_includes_the_seat_trees_reviews_beside_the_branchs() {
        let mock = Arc::new(MockReviewFacade::new());
        mock.summaries.lock().push(summary("branch-r1", 1));
        mock.tree_summaries.lock().push(summary("tree-r1", 1));
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "both scopes' rows are a healthy list: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        let ids: Vec<&str> = parsed
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|row| row["review_id"].as_str())
            .collect();
        assert!(
            ids.contains(&"branch-r1") && ids.contains(&"tree-r1"),
            "both scopes' rows are present: {ids:?}",
        );
        assert_eq!(
            mock.scopes_seen.lock().clone(),
            vec!["feat".to_owned(), "tree:TestOrg/test-project/caller".to_owned()],
            "the branch scope first, then the seat's tree",
        );
        let note = out.blocks.get(1).map_or("", block_text);
        assert!(note.contains("tree:TestOrg/test-project/caller"), "{note}");
    }

    /// An explicit scope means that scope alone: a document's hand-off
    /// asks for the document, never the branch-and-tree pair.
    #[tokio::test]
    async fn review_list_with_an_explicit_scope_does_not_consult_the_seat_tree() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool
            .call(ToolInput { value: serde_json::json!({ "scope": "doc:/proj/plan.md" }) })
            .await;
        assert!(!out.is_error, "an empty document scope is a healthy empty list");
        assert_eq!(
            mock.scopes_seen.lock().clone(),
            vec!["doc:/proj/plan.md".to_owned()],
            "the tree is not consulted beside an explicit scope",
        );
    }

    /// A tree read that failed never folds into an empty answer: the
    /// branch's rows still return, with the failure stated beside them.
    #[tokio::test]
    async fn review_list_states_a_failed_tree_read_beside_the_branch_rows() {
        let mock = Arc::new(MockReviewFacade::new());
        mock.summaries.lock().push(summary("branch-r1", 1));
        *mock.tree_list_error.lock() = Some("the store row would not decode".to_owned());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "the branch's rows still return: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed[0]["review_id"], "branch-r1");
        let note = out.blocks.get(1).map_or("", block_text);
        assert!(note.contains("could not be read"), "{note}");
        assert!(note.contains("would not decode"), "{note}");
    }

    /// A review the branch does not hold is found on the seat's tree.
    #[tokio::test]
    async fn review_get_falls_back_to_the_seat_tree() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.tree_detail.lock() = detail(&[review("tree-r1", 1, None)], &[], "tree-r1");
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewGet { facade, slot: caller_slot() };
        let out =
            tool.call(ToolInput { value: serde_json::json!({ "review_id": "tree-r1" }) }).await;
        assert!(!out.is_error, "the tree's review is found: {:?}", out.blocks);
        assert!(text_of(&out).contains("tree-r1"), "{}", text_of(&out));
        assert_eq!(
            mock.scopes_seen.lock().clone(),
            vec!["feat".to_owned(), "tree:TestOrg/test-project/caller".to_owned()],
            "the branch is tried first, then the tree",
        );
    }

    /// Not on the branch AND not on the tree names both, so the absence
    /// is stated rather than guessed at.
    #[tokio::test]
    async fn review_get_not_found_names_branch_and_tree() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewGet { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "review_id": "nope" }) }).await;
        assert!(out.is_error);
        assert!(text_of(&out).contains("or your seat tree"), "{}", text_of(&out));
    }

    /// A reply to a comment the branch's scope does not hold lands on the
    /// seat's tree instead - the files page's comments are answered the
    /// same way the branch's are.
    #[tokio::test]
    async fn review_reply_falls_back_to_the_seat_tree() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.fail_non_tree.lock() = true;
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, slot: caller_slot() };
        let out = tool
            .call(ToolInput { value: serde_json::json!({ "comment_id": "c1", "text": "done" }) })
            .await;
        assert!(!out.is_error, "the tree holds the comment: {:?}", out.blocks);
        assert_eq!(mock.reply_calls.lock().clone(), vec![("c1".to_owned(), "done".to_owned())],);
        assert_eq!(
            mock.scopes_seen.lock().clone(),
            vec!["feat".to_owned(), "tree:TestOrg/test-project/caller".to_owned()],
        );
    }

    /// review__resolve gets the same fallback, and a comment held by
    /// neither scope still answers with the branch's own error.
    #[tokio::test]
    async fn review_resolve_falls_back_and_keeps_the_branch_error_otherwise() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.fail_non_tree.lock() = true;
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewResolve { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c1" }) }).await;
        assert!(!out.is_error, "the tree holds the comment: {:?}", out.blocks);
        assert_eq!(mock.resolve_calls.lock().clone(), vec!["c1".to_owned()]);

        // A second mock where NEITHER scope holds it: the error returns.
        let mock = Arc::new(MockReviewFacade::new());
        mock.force_error.lock().replace("comment c9 is not in this scope".to_owned());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewResolve { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c9" }) }).await;
        assert!(out.is_error);
        assert!(text_of(&out).contains("not in this scope"), "{}", text_of(&out));
    }

    /// The other-branches hint never lists scope keys as branches: a tree
    /// or document scope in it would read as something to check out.
    #[tokio::test]
    async fn review_list_hint_never_lists_scope_keys_as_branches() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.review_branches.lock() = vec![
            "feat".to_owned(),
            "tree:TestOrg/test-project/caller".to_owned(),
            "doc:/proj/plan.md".to_owned(),
            "worktree-impl".to_owned(),
        ];
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        let text = text_of(&out);
        assert!(text.contains("worktree-impl"), "{text}");
        assert!(!text.contains("tree:"), "a scope key is not a branch: {text}");
        assert!(!text.contains("doc:"), "a scope key is not a branch: {text}");
    }

    #[tokio::test]
    async fn review_list_non_empty_says_nothing_when_no_other_branch_has_reviews() {
        let mock = Arc::new(MockReviewFacade::new());
        mock.summaries.lock().push(summary("r1", 1));
        *mock.review_branches.lock() = vec!["feat".to_owned()];
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert_eq!(out.blocks.len(), 1, "a normal list stays one json block: {:?}", out.blocks);
    }

    #[tokio::test]
    async fn review_list_empty_stays_an_empty_list_with_nothing_elsewhere() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewList { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!out.is_error, "a project with no reviews anywhere is not an error");
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
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
        let tool = ReviewGet { facade, slot: caller_slot() };
        let hit = tool.call(ToolInput { value: serde_json::json!({ "review_id": "r1" }) }).await;
        assert!(!hit.is_error);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&hit)).expect("json");
        assert_eq!(parsed["review_id"], "r1");
        let miss = tool.call(ToolInput { value: serde_json::json!({ "review_id": "r9" }) }).await;
        assert!(miss.is_error, "an unknown review_id is an error");
    }

    #[tokio::test]
    async fn review_reply_captures_and_returns_status() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, slot: caller_slot() };
        let out = tool
            .call(ToolInput {
                value: serde_json::json!({ "comment_id": "c1", "text": "fixed it" }),
            })
            .await;
        assert!(!out.is_error, "reply happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed["status"], "addressed");
        assert_eq!(parsed["comment_id"], "c1");
        let calls = mock.reply_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], ("c1".to_owned(), "fixed it".to_owned()));
    }

    /// The reply result carries the anchor, so the caller can say which
    /// spot the reply landed on; `{comment_id, status}` could only print
    /// an opaque id where the code's own location belongs.
    #[tokio::test]
    async fn review_reply_echoes_the_anchor() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.reply_anchor.lock() = CommentAnchorView {
            comment_id: "c-71".to_owned(),
            file: "src/chat/units.ts".to_owned(),
            line: 919,
            side: "new",
            status: "addressed",
            number: Some(3),
        };
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, slot: caller_slot() };

        let out = tool
            .call(ToolInput { value: serde_json::json!({ "comment_id": "c-71", "text": "fixed" }) })
            .await;
        assert!(!out.is_error, "reply happy path: {:?}", out.blocks);
        assert_eq!(out.blocks.len(), 1, "the anchor stays one text block: {out:?}");
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed["comment_id"], "c-71");
        assert_eq!(parsed["file"], "src/chat/units.ts");
        assert_eq!(parsed["line"], 919);
        assert_eq!(parsed["side"], "new");
        assert_eq!(parsed["number"], 3);
        assert_eq!(parsed["status"], "addressed");
    }

    /// A comment filed in no round yet carries a null `number`: the key is
    /// always there, so a reader never has to tell an absent key from an
    /// unfiled comment.
    #[tokio::test]
    async fn a_comment_in_no_round_reports_a_null_number() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.reply_anchor.lock() = CommentAnchorView {
            comment_id: "c-9".to_owned(),
            file: "src/x.rs".to_owned(),
            line: 1,
            side: "new",
            status: "addressed",
            number: None,
        };
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, slot: caller_slot() };

        let out = tool
            .call(ToolInput { value: serde_json::json!({ "comment_id": "c-9", "text": "x" }) })
            .await;
        assert!(!out.is_error, "reply happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed["number"], serde_json::Value::Null, "the key is present and null");
        assert!(parsed.get("number").is_some(), "not dropped from the shape: {parsed}");
    }

    /// A resolve echoes the same anchor: the row says which comment, on
    /// which spot, is now resolved.
    #[tokio::test]
    async fn review_resolve_echoes_the_anchor() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.resolve_anchor.lock() = CommentAnchorView {
            comment_id: "c-68".to_owned(),
            file: "src/chat/Inbound.svelte".to_owned(),
            line: 48,
            side: "new",
            status: "resolved",
            number: Some(3),
        };
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewResolve { facade, slot: caller_slot() };

        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c-68" }) }).await;
        assert!(!out.is_error, "resolve happy path: {:?}", out.blocks);
        assert_eq!(out.blocks.len(), 1, "the anchor stays one text block: {out:?}");
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed["comment_id"], "c-68");
        assert_eq!(parsed["file"], "src/chat/Inbound.svelte");
        assert_eq!(parsed["line"], 48);
        assert_eq!(parsed["side"], "new");
        assert_eq!(parsed["number"], 3);
        assert_eq!(parsed["status"], "resolved");
    }

    #[tokio::test]
    async fn review_reply_scope_rejection_surfaces_error() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.force_error.lock() = Some("no review comment c1 on (forge, feat)".to_owned());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewReply { facade, slot: caller_slot() };
        let out = tool
            .call(ToolInput { value: serde_json::json!({ "comment_id": "c1", "text": "x" }) })
            .await;
        assert!(out.is_error, "a cross-scope / unknown comment id must error");
        assert!(text_of(&out).contains("no review comment"));
        assert_eq!(mock.reply_calls.lock().len(), 0, "a rejected reply captures no call");
    }

    #[tokio::test]
    async fn review_resolve_captures_comment_id() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let tool = ReviewResolve { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c2" }) }).await;
        assert!(!out.is_error, "resolve happy path: {:?}", out.blocks);
        let parsed: serde_json::Value = serde_json::from_str(text_of(&out)).expect("json");
        assert_eq!(parsed["status"], "resolved");
        assert_eq!(*mock.resolve_calls.lock(), vec!["c2".to_owned()]);
    }

    /// The read-back for a document's review: `scope` on any of the four
    /// tools addresses that scope instead of the caller's branch, so a
    /// review filed under `doc:<path>` is listable, readable, answerable
    /// and resolvable from the session that wrote the document.
    #[tokio::test]
    async fn the_scope_argument_addresses_all_four_tools() {
        let mock = Arc::new(MockReviewFacade::new());
        *mock.detail.lock() = Some(ReviewDetail {
            review_id: "r1".to_owned(),
            number: 1,
            summary: None,
            comments: Vec::new(),
        });
        let facade: Arc<dyn ReviewFacade> = mock.clone();
        let scope = "doc:/proj/plan.md";

        let outputs = vec![
            ReviewList { facade: Arc::clone(&facade), slot: caller_slot() }
                .call(ToolInput { value: serde_json::json!({ "scope": scope }) })
                .await,
            ReviewGet { facade: Arc::clone(&facade), slot: caller_slot() }
                .call(ToolInput {
                    value: serde_json::json!({ "review_id": "r1", "scope": scope }),
                })
                .await,
            ReviewReply { facade: Arc::clone(&facade), slot: caller_slot() }
                .call(ToolInput {
                    value: serde_json::json!({ "comment_id": "c1", "text": "done", "scope": scope }),
                })
                .await,
            ReviewResolve { facade: Arc::clone(&facade), slot: caller_slot() }
                .call(ToolInput { value: serde_json::json!({ "comment_id": "c2", "scope": scope }) })
                .await,
        ];
        for out in &outputs {
            assert!(!out.is_error, "{:?}", out.blocks);
        }
        assert_eq!(
            mock.scopes_seen.lock().as_slice(),
            [scope, scope, scope, scope],
            "every tool addressed the explicit scope rather than the caller's branch",
        );
    }

    #[tokio::test]
    async fn review_reply_invalid_args_is_error() {
        let mock = Arc::new(MockReviewFacade::new());
        let facade: Arc<dyn ReviewFacade> = mock;
        let tool = ReviewReply { facade, slot: caller_slot() };
        let out = tool.call(ToolInput { value: serde_json::json!({ "comment_id": "c1" }) }).await;
        assert!(out.is_error, "missing 'text' is an error");
        assert!(text_of(&out).to_lowercase().contains("invalid"));
    }

    /// The document read-back loop, asserted by OUTCOME: with the
    /// document's scope as the only address, the four tools list, read,
    /// reply and resolve the review the viewer filed, and the STORE is
    /// what must show it - the terminal status on disk, not a tool call
    /// having appeared.
    #[tokio::test]
    async fn a_documents_review_reads_back_by_its_scope_to_a_resolved_thread() {
        use crate::workspace::Workspace;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.projects.push(crate::config::LoadedProject {
            name: "myproj".to_owned(),
            path: dir.path().to_path_buf(),
            display_path: dir.path().to_string_lossy().into_owned(),
            org: "TestOrg".to_owned(),
            accounts: Vec::new(),
            fallback_accounts: Vec::new(),
            auto_start: false,
            model: None,
            env: std::collections::HashMap::new(),
            max_workers: None,
            permission_mode: forge_primitives::permission::PermissionMode::Auto,
            vault_roots: Vec::new(),
        });
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
            .expect("the stub config is well-formed");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        // The viewer's side: a comment on a line of the document, filed
        // as one review under the document's own scope.
        let scope = "doc:/proj/plan.md";
        let mut t = thread("a", None, ReviewStatus::Open);
        t.anchor.base_ref = scope.to_owned();
        assert!(ws.upsert_review_thread("myproj", scope, t), "the thread lands");
        let reviewer = SessionSlot::from_str_for_test("reviewer-uuid");
        ws.submit_review("myproj", scope, Some("overview".to_owned()), &["a".to_owned()], reviewer)
            .expect("the review is sealed");

        // The agent's side, through the real tools over the real store.
        let facade: Arc<dyn ReviewFacade> = ProdReviewFacade::from_arc(&ws);
        let slot = SessionSlot::lead("TestOrg", "myproj");

        let list = ReviewList { facade: Arc::clone(&facade), slot: slot.clone() }
            .call(ToolInput { value: serde_json::json!({ "scope": scope }) })
            .await;
        assert!(!list.is_error, "{:?}", list.blocks);
        let rows: serde_json::Value = serde_json::from_str(text_of(&list)).expect("list json");
        let review_id = rows[0]["review_id"].as_str().expect("a review id").to_owned();

        let get = ReviewGet { facade: Arc::clone(&facade), slot: slot.clone() }
            .call(ToolInput {
                value: serde_json::json!({ "review_id": review_id, "scope": scope }),
            })
            .await;
        assert!(!get.is_error, "{:?}", get.blocks);
        let detail: serde_json::Value = serde_json::from_str(text_of(&get)).expect("get json");
        let comment_id =
            detail["comments"][0]["comment_id"].as_str().expect("a comment id").to_owned();
        assert_eq!(detail["comments"][0]["status"], "open", "the viewer's comment starts open");

        let reply = ReviewReply { facade: Arc::clone(&facade), slot: slot.clone() }
            .call(ToolInput {
                value: serde_json::json!({ "comment_id": comment_id, "text": "done", "scope": scope }),
            })
            .await;
        assert!(!reply.is_error, "{:?}", reply.blocks);
        let echoed: serde_json::Value = serde_json::from_str(text_of(&reply)).expect("reply json");
        assert_eq!(echoed["status"], "addressed", "a reply flips the comment");

        let resolve = ReviewResolve { facade: Arc::clone(&facade), slot }
            .call(ToolInput {
                value: serde_json::json!({ "comment_id": comment_id, "scope": scope }),
            })
            .await;
        assert!(!resolve.is_error, "{:?}", resolve.blocks);

        // The outcome, read from the store where it landed.
        let stored = ws.load_review_threads("myproj", scope).expect("threads read back");
        assert_eq!(
            stored[0].status,
            ReviewStatus::Resolved,
            "the stored thread is the assertion; a call that failed cannot read as resolved",
        );
    }

    #[test]
    fn build_server_registers_all_four_tools() {
        let facade = MockReviewFacade::new().into_arc();
        let server = build_server(facade, caller_slot());
        let debug = format!("{server:?}");
        for expected in ["review__list", "review__get", "review__reply", "review__resolve"] {
            assert!(debug.contains(expected), "build_server must include {expected}; {debug}");
        }
    }
}
