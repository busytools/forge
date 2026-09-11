//! Slack MCP - read the conversations a configured workspace offers
//! (`mcp__forge__slack__*`).
//!
//! `slack__list` lists every conversation in a workspace and marks which
//! ones the caller has subscribed; `slack__subscribe` and
//! `slack__unsubscribe` manage those records. Nothing is delivered yet -
//! the poll pump arrives in phase 3, so a record is inert until then.
//!
//! - [`facade`] - the `SlackFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

pub(crate) mod facade;

use std::sync::Arc;

use forge_primitives::slack::{SlackConversation, SlackSubscriptionTarget, SlackWatchMode};
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
use uuid::Uuid;

use crate::mcp::peers::facade::CallerKeyResolver;
use crate::mcp::slack::facade::{
    SlackAttachmentError, SlackChannelWatch, SlackEditError, SlackEditRequest, SlackFacade,
    SlackFetchRequest, SlackListError, SlackPostError, SlackPostRequest, SlackReactError,
    SlackReactRequest, SlackReadError, SlackSubscribeError, SlackSubscribeRequest,
    SlackUploadRequest,
};

/// Attach the Slack tools to an existing [`McpServerBuilder`]. Called for
/// BOTH lead and worker sessions (any-caller), so `build_forge_server`
/// invokes this unconditionally. `subscribe` / `unsubscribe` are scoped
/// to the caller's own records, so they need the caller key.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    let list = List { facade: facade.clone(), caller_key: caller_key.clone() };
    let subscribe = Subscribe { facade: facade.clone(), caller_key: caller_key.clone() };
    let unsubscribe = Unsubscribe { facade: facade.clone(), caller_key: caller_key.clone() };
    let post = Post { facade: facade.clone(), caller_key: caller_key.clone() };
    let edit = Edit { facade: facade.clone(), caller_key: caller_key.clone() };
    let react = React { facade: facade.clone(), caller_key: caller_key.clone() };
    let attachment = Attachment { facade: facade.clone(), caller_key };
    let search = Search { facade: facade.clone() };
    let user = User { facade: facade.clone() };
    let pins = Pins { facade: facade.clone() };
    let bookmarks = Bookmarks { facade };
    builder
        .tool(list)
        .tool(subscribe)
        .tool(unsubscribe)
        .tool(post)
        .tool(edit)
        .tool(react)
        .tool(attachment)
        .tool(search)
        .tool(user)
        .tool(pins)
        .tool(bookmarks)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

/// The conversation's name, or the DM partner's user id when Slack omits
/// one - a DM's display name is otherwise empty.
fn conversation_name(conversation: &SlackConversation) -> String {
    conversation.name.clone().or_else(|| conversation.user.clone()).unwrap_or_default()
}

fn conversation_kind(conversation: &SlackConversation) -> &'static str {
    if conversation.is_im {
        "im"
    } else if conversation.is_mpim {
        "mpim"
    } else if conversation.is_private {
        "private"
    } else {
        "public"
    }
}

/// A conversation is watched when a target names its id, or - for a DM
/// or group DM - when the caller watches the whole DM class.
fn is_subscribed(conversation: &SlackConversation, subscribed: &[SlackSubscriptionTarget]) -> bool {
    subscribed.iter().any(|target| match target {
        SlackSubscriptionTarget::DirectMessages => conversation.is_im || conversation.is_mpim,
        SlackSubscriptionTarget::Conversation { id, .. } => id == &conversation.id,
        // A mention target is not a conversation subscription, so it
        // marks no row.
        SlackSubscriptionTarget::Mentions => false,
    })
}

/// One row per conversation, marking the ones in `subscribed`.
fn list_rows(
    conversations: &[SlackConversation],
    subscribed: &[SlackSubscriptionTarget],
) -> Vec<serde_json::Value> {
    conversations
        .iter()
        .map(|conversation| {
            serde_json::json!({
                "id": conversation.id,
                "name": conversation_name(conversation),
                "kind": conversation_kind(conversation),
                "subscribed": is_subscribed(conversation, subscribed),
            })
        })
        .collect()
}

/// Whether one conversation passes `slack__list`'s optional filters: a
/// case-insensitive substring over the display name, purpose and topic,
/// and an exact kind match.
fn passes_filter(conversation: &SlackConversation, name: Option<&str>, kind: Option<&str>) -> bool {
    if let Some(kind) = kind
        && conversation_kind(conversation) != kind
    {
        return false;
    }
    let Some(needle) = name else { return true };
    let needle = needle.to_lowercase();
    [conversation_name(conversation).to_lowercase()]
        .into_iter()
        .chain(conversation.purpose.iter().map(|p| p.value.to_lowercase()))
        .chain(conversation.topic.iter().map(|t| t.value.to_lowercase()))
        .any(|haystack| haystack.contains(&needle))
}

fn format_list_error(err: &SlackListError) -> String {
    match err {
        SlackListError::NotConfigured => {
            "no Slack workspace configured in forge.toml [[slack]]".to_owned()
        }
        SlackListError::UnknownWorkspace { requested, known } => {
            format!("no Slack workspace named '{requested}'; configured: {}", known.join(", "))
        }
        SlackListError::WorkspaceRequired { known } => {
            format!(
                "several Slack workspaces are configured; pass `workspace`: {}",
                known.join(", ")
            )
        }
        SlackListError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

struct List {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct ListArgs {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// The kinds `kind` accepts, matching what each row reports.
const LIST_KINDS: &str = "public, private, im, mpim";

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "slack__list"
    }

    fn description(&self) -> &'static str {
        "List every conversation in a Slack workspace - public and private channels, DMs and \
         group DMs the token's user is a member of - each row marked with whether YOU are \
         subscribed, counting only your own subscriptions rather than another session's. Pass \
         `workspace` to choose one; omit it when only one is configured. Pass `name` for only \
         conversations whose name, purpose or topic contains that text (case-insensitive), or \
         `kind` for only one conversation type. Returns a JSON array of \
         {id, name, kind, subscribed}. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label to list. Omit it when only \
                                    one workspace is configured.",
                },
                "name": {
                    "type": "string",
                    "description": "Only conversations whose name, purpose or topic contains \
                                    this text (case-insensitive).",
                },
                "kind": {
                    "type": "string",
                    "enum": ["public", "private", "im", "mpim"],
                    "description": "Only conversations of this kind, matching the `kind` field \
                                    in each row.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ListArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        if let Some(kind) = args.kind.as_deref()
            && !matches!(kind, "public" | "private" | "im" | "mpim")
        {
            return tool_error(format!("unknown kind '{kind}'; the kinds are {LIST_KINDS}"));
        }
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        match self.facade.conversations(args.workspace.as_deref()).await {
            Ok(conversations) => {
                let subscribed = self.facade.subscribed_targets(&caller, args.workspace.as_deref());
                let kept: Vec<SlackConversation> = conversations
                    .iter()
                    .filter(|conversation| {
                        passes_filter(conversation, args.name.as_deref(), args.kind.as_deref())
                    })
                    .cloned()
                    .collect();
                let rows = list_rows(&kept, &subscribed);
                match serde_json::to_string_pretty(&serde_json::Value::Array(rows)) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => {
                        tool_error(format!("conversation-list serialization failed: {err}"))
                    }
                }
            }
            Err(err) => tool_error(format_list_error(&err)),
        }
    }
}

/// The mode a caller writes in a `slack__subscribe` argument.
#[derive(serde::Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum WatchModeArg {
    #[default]
    All,
    Mentions,
}

impl From<WatchModeArg> for SlackWatchMode {
    fn from(arg: WatchModeArg) -> Self {
        match arg {
            WatchModeArg::All => Self::All,
            WatchModeArg::Mentions => Self::MentionsOnly,
        }
    }
}

#[derive(serde::Deserialize)]
struct ChannelWatchArg {
    id: String,
    #[serde(default)]
    mode: WatchModeArg,
}

fn format_subscribe_error(err: &SlackSubscribeError, requested: Option<&str>) -> String {
    match err {
        SlackSubscribeError::UnknownWorkspace => match requested {
            Some(label) => {
                format!("no Slack workspace named '{label}' is configured in forge.toml [[slack]]")
            }
            None => {
                "several Slack workspaces are configured; pass `workspace` naming one".to_owned()
            }
        },
        SlackSubscribeError::UnknownCallerProject => {
            "couldn't resolve your project; is this session attached to a forge.toml project?"
                .to_owned()
        }
    }
}

struct Subscribe {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct SubscribeArgs {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    direct_messages: Option<bool>,
    #[serde(default)]
    mentions: Option<bool>,
    #[serde(default)]
    conversations: Option<Vec<ChannelWatchArg>>,
}

#[async_trait::async_trait]
impl Tool for Subscribe {
    fn name(&self) -> &'static str {
        "slack__subscribe"
    }

    fn description(&self) -> &'static str {
        "Subscribe YOUR session to a Slack workspace: the whole DM class, being mentioned \
         anywhere in the workspace, or named conversations with a mode. A mention subscription \
         sees mentions in public channels you are not in as well as in every conversation you \
         are, but not in private channels you are not in - those you could not read anyway, and \
         search lag means a mention is not instantaneous. A conversation in `mentions` mode \
         delivers only messages that mention the user; `all` delivers every message. Pass \
         `workspace` to choose one, or omit it when only one is configured. Returns the new \
         subscription ids. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label to subscribe in. Omit it \
                                    when only one workspace is configured.",
                },
                "direct_messages": {
                    "type": "boolean",
                    "description": "Watch every DM in the workspace, group DMs included.",
                },
                "mentions": {
                    "type": "boolean",
                    "description": "Watch for being mentioned anywhere in the workspace, including \
                                    public channels you are not in. Not private channels you are \
                                    not in.",
                },
                "conversations": {
                    "type": "array",
                    "description": "Conversations to watch, each with its own mode.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "The conversation id from slack__list.",
                            },
                            "mode": {
                                "type": "string",
                                "enum": ["all", "mentions"],
                                "description": "`all` delivers every message; `mentions` only \
                                                those that mention the user. Defaults to `all`.",
                            },
                        },
                        "required": ["id"],
                        "additionalProperties": false,
                    },
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: SubscribeArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let mut requests = Vec::new();
        if args.direct_messages.unwrap_or(false) {
            requests.push(SlackSubscribeRequest::DirectMessages);
        }
        if args.mentions.unwrap_or(false) {
            requests.push(SlackSubscribeRequest::Mentions);
        }
        if let Some(watches) = args.conversations.filter(|watches| !watches.is_empty()) {
            requests.push(SlackSubscribeRequest::Conversations(
                watches
                    .into_iter()
                    .map(|watch| SlackChannelWatch { id: watch.id, mode: watch.mode.into() })
                    .collect(),
            ));
        }
        if requests.is_empty() {
            return tool_error(
                "pass `direct_messages: true`, `mentions: true`, or a non-empty `conversations` \
                 array"
                    .to_owned(),
            );
        }
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        let mut ids: Vec<Uuid> = Vec::new();
        for request in requests {
            match self.facade.subscribe(&caller, args.workspace.as_deref(), request) {
                Ok(mut created) => ids.append(&mut created),
                Err(err) => {
                    return tool_error(format_subscribe_error(&err, args.workspace.as_deref()));
                }
            }
        }
        ToolOutput::text(format!("subscribed to Slack ({})", ids.len()))
    }
}

struct Unsubscribe {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct UnsubscribeArgs {
    id: String,
}

#[async_trait::async_trait]
impl Tool for Unsubscribe {
    fn name(&self) -> &'static str {
        "slack__unsubscribe"
    }

    fn description(&self) -> &'static str {
        "Remove one of YOUR OWN Slack subscriptions by id (from slack__subscribe / \
         slack__list), scoped to what you subscribed - a caller manages only what it created. \
         Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The subscription id to remove." },
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: UnsubscribeArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let Ok(id) = Uuid::parse_str(&args.id) else {
            return tool_error(format!("not a valid subscription id: {}", args.id));
        };
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        if self.facade.unsubscribe(&caller, id) {
            ToolOutput::text(format!("unsubscribed {id}"))
        } else {
            tool_error(format!("no subscription with id {id} in your project"))
        }
    }
}

fn format_post_error(err: &SlackPostError) -> String {
    match err {
        SlackPostError::Rejected => "the post was not approved, so nothing was sent".to_owned(),
        SlackPostError::UnknownWorkspace => {
            "no Slack workspace by that name is configured in forge.toml [[slack]]".to_owned()
        }
        SlackPostError::Partial { posted, total, source } => {
            format!("posted part {posted} of {total}; the rest were NOT sent ({source})")
        }
        SlackPostError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

fn format_edit_error(err: &SlackEditError) -> String {
    match err {
        SlackEditError::NotOwnMessage => {
            "that message was not posted by you, so Slack will not let you change it".to_owned()
        }
        SlackEditError::Rejected => {
            "the replacement was not approved, so the message is untouched".to_owned()
        }
        SlackEditError::UnknownWorkspace => {
            "no Slack workspace by that name is configured in forge.toml [[slack]]".to_owned()
        }
        SlackEditError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

fn format_react_error(err: &SlackReactError) -> String {
    match err {
        SlackReactError::Rejected => {
            "the reaction was not approved, so nothing was added or removed".to_owned()
        }
        SlackReactError::UnknownWorkspace => {
            "no Slack workspace by that name is configured in forge.toml [[slack]]".to_owned()
        }
        SlackReactError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

struct Post {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct PostArgs {
    #[serde(default)]
    workspace: Option<String>,
    conversation: String,
    #[serde(default)]
    thread_ts: Option<String>,
    text: String,
}

#[async_trait::async_trait]
impl Tool for Post {
    fn name(&self) -> &'static str {
        "slack__post"
    }

    fn description(&self) -> &'static str {
        "Post a message to Slack as the user - a root message, or a reply into an existing \
         thread when `thread_ts` is passed. This is HELD FOR APPROVAL: the call does not return \
         until the user decides in the dock prompt, and a rejected or unanswered draft posts \
         nothing. Text past 4000 characters is split into numbered parts automatically. Returns \
         how many messages were posted. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label to post in. Omit it when \
                                    only one workspace is configured.",
                },
                "conversation": {
                    "type": "string",
                    "description": "The conversation id from slack__list, or a user id to open \
                                    a DM with.",
                },
                "thread_ts": {
                    "type": "string",
                    "description": "The parent message's ts to reply into that thread. Omit to \
                                    post a root message.",
                },
                "text": { "type": "string", "description": "The message body." },
            },
            "required": ["conversation", "text"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: PostArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        let request = SlackPostRequest {
            workspace: args.workspace,
            conversation: args.conversation,
            thread_ts: args.thread_ts,
            text: args.text,
        };
        match self.facade.post(&caller, request).await {
            Ok(outcome) => {
                ToolOutput::text(format!("posted to Slack ({} message(s))", outcome.posted))
            }
            Err(err) => tool_error(format_post_error(&err)),
        }
    }
}

struct Edit {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct EditArgs {
    #[serde(default)]
    workspace: Option<String>,
    conversation: String,
    ts: String,
    #[serde(default)]
    delete: Option<bool>,
    #[serde(default)]
    text: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "slack__edit"
    }

    fn description(&self) -> &'static str {
        "Update or delete one of YOUR OWN Slack messages: pass `text` to replace its body, or \
         `delete: true` to remove it. BOTH ARE HELD FOR APPROVAL like slack__post - a \
         replacement puts new words in front of people as you, and a deletion changes what they \
         see on a message attributed to you and cannot be undone by editing again. The call does \
         not return until the user decides, and a rejected or unanswered draft leaves the \
         message untouched. Slack refuses a message you did not post, and this refuses it \
         locally so the reason is clear. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "conversation": { "type": "string", "description": "The conversation id." },
                "ts": { "type": "string", "description": "The message's ts." },
                "text": {
                    "type": "string",
                    "description": "The replacement body. Omit when deleting.",
                },
                "delete": {
                    "type": "boolean",
                    "description": "Set true to delete the message instead of updating it.",
                },
            },
            "required": ["conversation", "ts"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: EditArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let delete = args.delete.unwrap_or(false);
        if !delete && args.text.is_none() {
            return tool_error("pass `text` to update the message, or `delete: true`".to_owned());
        }
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        let request = SlackEditRequest {
            workspace: args.workspace,
            conversation: args.conversation,
            ts: args.ts,
            text: if delete { None } else { args.text },
        };
        match self.facade.edit(&caller, request).await {
            Ok(()) => {
                ToolOutput::text(if delete { "deleted".to_owned() } else { "updated".to_owned() })
            }
            Err(err) => tool_error(format_edit_error(&err)),
        }
    }
}

struct React {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct ReactArgs {
    #[serde(default)]
    workspace: Option<String>,
    conversation: String,
    ts: String,
    name: String,
    #[serde(default)]
    remove: Option<bool>,
}

#[async_trait::async_trait]
impl Tool for React {
    fn name(&self) -> &'static str {
        "slack__react"
    }

    fn description(&self) -> &'static str {
        "Add a reaction to a Slack message, or remove one with `remove: true`. `name` is Slack's \
         shortcode without colons, e.g. `white_check_mark`. Removing requires being the original \
         reaction's author. NOT HELD FOR APPROVAL. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "conversation": { "type": "string", "description": "The conversation id." },
                "ts": { "type": "string", "description": "The message's ts." },
                "name": {
                    "type": "string",
                    "description": "Slack reaction shortcode without colons, e.g. \
                                    `white_check_mark`.",
                },
                "remove": {
                    "type": "boolean",
                    "description": "Set true to remove the reaction instead of adding it.",
                },
            },
            "required": ["conversation", "ts", "name"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ReactArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        let request = SlackReactRequest {
            workspace: args.workspace,
            conversation: args.conversation,
            ts: args.ts,
            name: args.name,
            add: !args.remove.unwrap_or(false),
        };
        match self.facade.react(&caller, request).await {
            Ok(()) => ToolOutput::text("reaction applied".to_owned()),
            Err(err) => tool_error(format_react_error(&err)),
        }
    }
}

fn format_attachment_error(err: &SlackAttachmentError) -> String {
    match err {
        SlackAttachmentError::Io(message) => format!("file error: {message}"),
        SlackAttachmentError::Rejected => {
            "the upload was not approved, so nothing was sent".to_owned()
        }
        SlackAttachmentError::UnknownWorkspace => {
            "no Slack workspace by that name is configured in forge.toml [[slack]]".to_owned()
        }
        SlackAttachmentError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

struct Attachment {
    facade: Arc<dyn SlackFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct AttachmentArgs {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    file_id: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    conversation: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Attachment {
    fn name(&self) -> &'static str {
        "slack__attachment"
    }

    fn description(&self) -> &'static str {
        "Move a file between Slack and the local machine. Pass `file_id` with `dir` to fetch a \
         file, and it lands in that directory under the uploader's file name (sanitised, so it \
         cannot escape the directory); the returned path is where it landed. Pass `path` with \
         `conversation` to upload a local file into that conversation, optionally into a thread \
         with `thread_ts`. An upload is HELD FOR APPROVAL like slack__post, because it posts new \
         content; fetching is not. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "file_id": {
                    "type": "string",
                    "description": "The Slack file id to fetch. Pair it with `dir`.",
                },
                "dir": {
                    "type": "string",
                    "description": "The local directory a fetched file lands in.",
                },
                "path": {
                    "type": "string",
                    "description": "The local file to upload. Pair it with `conversation`.",
                },
                "conversation": {
                    "type": "string",
                    "description": "The conversation id an uploaded file goes into.",
                },
                "thread_ts": {
                    "type": "string",
                    "description": "The parent ts to upload into that thread. Omit to post a \
                                    root message.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: AttachmentArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match (args.file_id, args.path) {
            (Some(file_id), _) => {
                let Some(dir) = args.dir else {
                    return tool_error(
                        "pass `dir`, the directory the fetched file should land in".to_owned(),
                    );
                };
                let request = SlackFetchRequest {
                    workspace: args.workspace,
                    file_id,
                    dir: std::path::PathBuf::from(dir),
                };
                match self.facade.fetch_attachment(request).await {
                    Ok(path) => ToolOutput::text(format!("fetched to {}", path.display())),
                    Err(err) => tool_error(format_attachment_error(&err)),
                }
            }
            (None, Some(path)) => {
                let Some(conversation) = args.conversation else {
                    return tool_error(
                        "pass `conversation`, the conversation to upload into".to_owned(),
                    );
                };
                let caller = match self.caller_key.current() {
                    Ok(key) => key,
                    Err(err) => return tool_error(err.to_string()),
                };
                let request = SlackUploadRequest {
                    workspace: args.workspace,
                    conversation,
                    thread_ts: args.thread_ts,
                    path: std::path::PathBuf::from(path),
                    title: None,
                };
                match self.facade.post_attachment(&caller, request).await {
                    Ok(()) => ToolOutput::text("uploaded".to_owned()),
                    Err(err) => tool_error(format_attachment_error(&err)),
                }
            }
            (None, None) => tool_error(
                "pass `file_id` and `dir` to fetch a file, or `path` and `conversation` to upload \
                 one"
                .to_owned(),
            ),
        }
    }
}

fn format_read_error(err: &SlackReadError) -> String {
    match err {
        SlackReadError::UnknownWorkspace => {
            "no Slack workspace by that name is configured in forge.toml [[slack]]".to_owned()
        }
        SlackReadError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

/// Results per `slack__search` when the caller does not cap them.
const DEFAULT_SEARCH_COUNT: u32 = 20;

struct Search {
    facade: Arc<dyn SlackFacade>,
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    #[serde(default)]
    workspace: Option<String>,
    query: String,
    #[serde(default)]
    count: Option<u32>,
}

#[async_trait::async_trait]
impl Tool for Search {
    fn name(&self) -> &'static str {
        "slack__search"
    }

    fn description(&self) -> &'static str {
        "Search a Slack workspace's messages by text, newest first. This is a LOOKUP, not a \
         mention detector: it matches the words you pass and cannot find every message that \
         mentions a user - subscribe to the mention target for that. Returns a JSON array of \
         {ts, text, conversation, conversation_name, username}; `conversation_name` is null for \
         a DM, which has none. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "query": {
                    "type": "string",
                    "description": "Slack search syntax. Matches text; it is not a mention \
                                    filter.",
                },
                "count": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum results (default 20).",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: SearchArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let count = args.count.unwrap_or(DEFAULT_SEARCH_COUNT);
        match self.facade.search(args.workspace.as_deref(), &args.query, count).await {
            Ok(found) => {
                let rows: Vec<serde_json::Value> = found
                    .iter()
                    .map(|hit| {
                        serde_json::json!({
                            "ts": hit.ts,
                            "text": hit.text,
                            "conversation": hit.conversation_id,
                            "conversation_name": hit.conversation_name,
                            "username": hit.username,
                        })
                    })
                    .collect();
                match serde_json::to_string_pretty(&serde_json::Value::Array(rows)) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("search serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct User {
    facade: Arc<dyn SlackFacade>,
}

#[derive(serde::Deserialize)]
struct UserArgs {
    #[serde(default)]
    workspace: Option<String>,
    user: String,
}

#[async_trait::async_trait]
impl Tool for User {
    fn name(&self) -> &'static str {
        "slack__user"
    }

    fn description(&self) -> &'static str {
        "Look up one Slack user by id: their handle, real name and timezone. Returns \
         {id, name, real_name, tz}, each of the last three null when the workspace does not \
         report it. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "user": { "type": "string", "description": "The user id, e.g. `U06MVPF6HU4`." },
            },
            "required": ["user"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: UserArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.user(args.workspace.as_deref(), &args.user).await {
            Ok(user) => {
                let body = serde_json::json!({
                    "id": user.id,
                    "name": user.name,
                    "real_name": user.real_name,
                    "tz": user.tz,
                });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("user serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct Pins {
    facade: Arc<dyn SlackFacade>,
}

#[derive(serde::Deserialize)]
struct PinsArgs {
    #[serde(default)]
    workspace: Option<String>,
    conversation: String,
}

#[async_trait::async_trait]
impl Tool for Pins {
    fn name(&self) -> &'static str {
        "slack__pins"
    }

    fn description(&self) -> &'static str {
        "List a Slack conversation's pinned messages - what the channel has kept as standing \
         context. Read-only and not held for approval. Pass `workspace` to choose one; omit it \
         when only one is configured. Returns a JSON array of {ts, user, text}. Any session in \
         the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "conversation": {
                    "type": "string",
                    "description": "The conversation id from slack__list.",
                },
            },
            "required": ["conversation"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: PinsArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.pins(args.workspace.as_deref(), &args.conversation).await {
            Ok(pins) => {
                let rows: Vec<serde_json::Value> = pins
                    .iter()
                    .filter_map(|pin| {
                        let message = pin.message.as_ref()?;
                        Some(serde_json::json!({
                            "ts": message.ts,
                            "user": message.user,
                            "text": message.text,
                        }))
                    })
                    .collect();
                match serde_json::to_string_pretty(&serde_json::Value::Array(rows)) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("pins serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct Bookmarks {
    facade: Arc<dyn SlackFacade>,
}

#[derive(serde::Deserialize)]
struct BookmarksArgs {
    #[serde(default)]
    workspace: Option<String>,
    conversation: String,
}

#[async_trait::async_trait]
impl Tool for Bookmarks {
    fn name(&self) -> &'static str {
        "slack__bookmarks"
    }

    fn description(&self) -> &'static str {
        "List a Slack conversation's bookmarks - the links saved on the channel. Read-only and \
         not held for approval. Pass `workspace` to choose one; omit it when only one is \
         configured. Returns a JSON array of {id, title, link}, each of the last two null when \
         the workspace does not report it. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "The `[[slack]]` workspace label. Omit it when only one \
                                    workspace is configured.",
                },
                "conversation": {
                    "type": "string",
                    "description": "The conversation id from slack__list.",
                },
            },
            "required": ["conversation"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: BookmarksArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.bookmarks(args.workspace.as_deref(), &args.conversation).await {
            Ok(bookmarks) => {
                let rows: Vec<serde_json::Value> = bookmarks
                    .iter()
                    .map(|bookmark| {
                        serde_json::json!({
                            "id": bookmark.id,
                            "title": bookmark.title,
                            "link": bookmark.link,
                        })
                    })
                    .collect();
                match serde_json::to_string_pretty(&serde_json::Value::Array(rows)) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("bookmarks serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::slack::facade::MockSlackFacade;
    use forge_primitives::slack::{
        SlackBookmark, SlackConversation, SlackConversationText, SlackPin, SlackPinMessage,
    };
    use std::sync::Arc;

    fn channel(id: &str, name: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            is_channel: true,
            is_private: false,
            is_im: false,
            is_mpim: false,
            is_archived: false,
            user: None,
            purpose: None,
            topic: None,
        }
    }

    fn dm(id: &str, user: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: None,
            is_channel: false,
            is_private: false,
            is_im: true,
            is_mpim: false,
            is_archived: false,
            user: Some(user.to_owned()),
            purpose: None,
            topic: None,
        }
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput { value }
    }

    fn watching(id: &str) -> SlackSubscriptionTarget {
        SlackSubscriptionTarget::Conversation { id: id.to_owned(), mode: SlackWatchMode::All }
    }

    #[test]
    fn list_marks_a_conversation_as_subscribed_only_when_it_is() {
        let conversations = vec![channel("C1", "general")];
        let rows = list_rows(&conversations, &[watching("C1")]);
        assert_eq!(rows.len(), 1, "one row per conversation");
        assert_eq!(rows[0]["id"], "C1");
        assert_eq!(rows[0]["name"], "general");
        assert_eq!(rows[0]["subscribed"], true);

        let rows = list_rows(&conversations, &[]);
        assert_eq!(rows[0]["subscribed"], false, "an empty subscribed set marks nothing");
    }

    #[test]
    fn a_watched_conversation_is_marked_and_an_unwatched_one_is_not() {
        let conversations = vec![channel("C1", "general"), channel("C2", "random")];
        let rows = list_rows(&conversations, &[watching("C1")]);
        let by_id =
            |id: &str| rows.iter().find(|row| row["id"] == id).expect("row for the id").clone();
        assert_eq!(by_id("C1")["subscribed"], true, "the watched channel is marked");
        assert_eq!(by_id("C2")["subscribed"], false, "its unwatched sibling is not");
    }

    #[test]
    fn the_dm_class_marks_every_dm_and_group_dm() {
        let conversations = vec![dm("D1", "U1"), dm("D2", "U2"), channel("C1", "general")];
        let rows = list_rows(&conversations, &[SlackSubscriptionTarget::DirectMessages]);
        let by_id =
            |id: &str| rows.iter().find(|row| row["id"] == id).expect("row for the id").clone();
        assert_eq!(by_id("D1")["subscribed"], true, "every DM is covered by the class");
        assert_eq!(by_id("D2")["subscribed"], true, "including the second one");
        assert_eq!(
            by_id("C1")["subscribed"],
            false,
            "a channel is not a DM, so the class does not mark it",
        );
    }

    #[test]
    fn a_dm_with_no_name_is_named_after_its_partner() {
        let rows = list_rows(&[dm("D1", "U9")], &[]);
        assert_eq!(rows[0]["name"], "U9", "a DM's display name falls back to the partner");
    }

    #[test]
    fn each_conversation_reports_its_kind() {
        let mut private = channel("G1", "secret");
        private.is_channel = false;
        private.is_private = true;
        let mut group_dm = channel("G2", "huddle");
        group_dm.is_channel = false;
        group_dm.is_mpim = true;

        let rows = list_rows(&[channel("C1", "general"), dm("D1", "U9"), private, group_dm], &[]);
        let kinds: Vec<&str> =
            rows.iter().map(|row| row["kind"].as_str().unwrap_or_default()).collect();
        assert_eq!(kinds, vec!["public", "im", "private", "mpim"]);
    }

    #[test]
    fn the_name_filter_matches_the_name_purpose_and_topic_case_insensitively() {
        let mut ch = channel("C1", "random");
        ch.purpose = Some(SlackConversationText { value: "Deploy chatter".to_owned() });
        ch.topic = Some(SlackConversationText { value: "release coordination".to_owned() });

        assert!(passes_filter(&ch, Some("rand"), None), "the name matches");
        assert!(passes_filter(&ch, Some("deploy"), None), "the purpose matches");
        assert!(passes_filter(&ch, Some("RELEASE"), None), "the topic matches, case-insensitively",);
        assert!(!passes_filter(&ch, Some("general"), None), "an unrelated needle matches nothing");
        assert!(
            passes_filter(&dm("D1", "U9"), Some("u9"), None),
            "a DM's display name is its partner id",
        );
    }

    #[test]
    fn the_kind_filter_keeps_only_that_kind() {
        let conversations = [channel("C1", "general"), dm("D1", "U9")];
        let kept: Vec<&SlackConversation> =
            conversations.iter().filter(|c| passes_filter(c, None, Some("im"))).collect();
        assert_eq!(kept.len(), 1, "one conversation survives the kind filter");
        assert_eq!(kept[0].id, "D1", "and it is the one of that kind");
    }

    #[test]
    fn each_list_error_names_its_cause() {
        let not_configured = format_list_error(&SlackListError::NotConfigured);
        assert!(not_configured.contains("[[slack]]"), "got: {not_configured}");

        let unknown = format_list_error(&SlackListError::UnknownWorkspace {
            requested: "nope".to_owned(),
            known: vec!["acme".to_owned(), "beta".to_owned()],
        });
        assert!(unknown.contains("nope"), "names what was asked for: {unknown}");
        assert!(unknown.contains("acme") && unknown.contains("beta"), "lists the known: {unknown}");

        let ambiguous = format_list_error(&SlackListError::WorkspaceRequired {
            known: vec!["acme".to_owned(), "beta".to_owned()],
        });
        assert!(ambiguous.contains("acme") && ambiguous.contains("beta"), "got: {ambiguous}");

        let fetch = format_list_error(&SlackListError::Fetch("boom".to_owned()));
        assert!(fetch.contains("boom"), "carries the cause: {fetch}");
    }

    #[tokio::test]
    async fn slack_list_returns_rows_and_passes_the_workspace_through() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Ok(vec![channel("C1", "general")]));
        let tool = List { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "workspace": "acme" }))).await;
        assert!(!out.is_error, "a configured workspace succeeds: {}", out.blocks[0].text);
        assert!(
            out.blocks[0].text.contains("C1"),
            "the row reaches the output: {}",
            out.blocks[0].text
        );
        assert_eq!(
            mock.conversations_calls.lock().as_slice(),
            [Some("acme".to_owned())],
            "the named workspace is the one queried",
        );
    }

    fn resolver() -> CallerKeyResolver {
        CallerKeyResolver::from_fixed(crate::SessionKey::from_session_id("caller"))
    }

    #[tokio::test]
    async fn slack_subscribe_passes_the_targets_through_to_the_facade() {
        let id = Uuid::from_u128(0x42);
        let mock = Arc::new(MockSlackFacade::new());
        *mock.subscribe_result.lock() = Some(Ok(vec![id]));
        let tool = Subscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool
            .call(input(serde_json::json!({
                "workspace": "acme",
                "conversations": [{ "id": "C1", "mode": "mentions" }],
            })))
            .await;
        assert!(!out.is_error, "a valid subscribe succeeds: {}", out.blocks[0].text);

        let calls = mock.subscribe_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0.as_deref(),
            Some("acme"),
            "the named workspace is the one written to"
        );
        assert_eq!(
            calls[0].1,
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode: SlackWatchMode::MentionsOnly,
            }]),
            "the channel and its mode survive the argument decode",
        );
    }

    #[tokio::test]
    async fn slack_subscribe_without_a_target_is_refused() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Subscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "workspace": "acme" }))).await;
        assert!(out.is_error, "a call naming nothing to watch must not create records");
        assert!(
            out.blocks[0].text.contains("direct_messages"),
            "the error says what to pass: {}",
            out.blocks[0].text,
        );
        assert!(mock.subscribe_calls.lock().is_empty(), "nothing reaches the facade");
    }

    #[tokio::test]
    async fn slack_subscribe_names_the_workspace_that_was_refused() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.subscribe_result.lock() = Some(Err(SlackSubscribeError::UnknownWorkspace));
        let tool = Subscribe { facade: mock, caller_key: resolver() };

        let out = tool
            .call(input(serde_json::json!({ "workspace": "nope", "direct_messages": true })))
            .await;
        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("nope"),
            "the error names the label that failed: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn slack_unsubscribe_refuses_an_id_the_caller_does_not_own() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.unsubscribe_result.lock() = Some(false);
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out =
            tool.call(input(serde_json::json!({ "id": Uuid::from_u128(0x9).to_string() }))).await;
        assert!(out.is_error, "an unremovable id signals an error to the LLM");
    }

    #[tokio::test]
    async fn slack_unsubscribe_rejects_a_bad_uuid_without_touching_the_facade() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "id": "not-a-uuid" }))).await;
        assert!(out.is_error);
        assert!(mock.unsubscribe_calls.lock().is_empty(), "a bad id never reaches the facade");
    }

    #[tokio::test]
    async fn slack_list_marks_what_the_caller_watches() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() =
            Some(Ok(vec![channel("C1", "general"), channel("C2", "random")]));
        *mock.subscribed_targets.lock() = vec![watching("C1")];
        let tool = List { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "workspace": "acme" }))).await;
        assert!(!out.is_error, "list succeeds: {}", out.blocks[0].text);
        let rows: serde_json::Value =
            serde_json::from_str(&out.blocks[0].text).expect("the output is JSON");
        let by_id = |id: &str| {
            rows.as_array()
                .expect("an array of rows")
                .iter()
                .find(|row| row["id"] == id)
                .expect("row for the id")
                .clone()
        };
        assert_eq!(by_id("C1")["subscribed"], true, "the caller's target reaches its row");
        assert_eq!(by_id("C2")["subscribed"], false, "an unwatched row stays unmarked");
    }

    #[tokio::test]
    async fn slack_list_filters_apply_before_the_rows_are_built() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() =
            Some(Ok(vec![channel("C1", "general"), dm("D1", "U9")]));
        let tool = List { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "kind": "im" }))).await;
        assert!(!out.is_error, "a known kind succeeds: {}", out.blocks[0].text);
        let rows: serde_json::Value =
            serde_json::from_str(&out.blocks[0].text).expect("the output is JSON");
        let ids: Vec<&str> = rows
            .as_array()
            .expect("an array of rows")
            .iter()
            .map(|row| row["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(ids, vec!["D1"], "the kind filter keeps only that kind");
        assert_eq!(
            mock.conversations_calls.lock().len(),
            1,
            "filtering is local; the workspace is queried once",
        );
    }

    #[tokio::test]
    async fn slack_list_refuses_an_unknown_kind() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = List { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "kind": "chanels" }))).await;
        assert!(out.is_error, "a typo must not read as an empty workspace");
        assert!(
            out.blocks[0].text.contains("chanels"),
            "the error names what was rejected: {}",
            out.blocks[0].text,
        );
        assert!(
            mock.conversations_calls.lock().is_empty(),
            "an invalid filter never reaches Slack",
        );
    }

    #[tokio::test]
    async fn slack_search_passes_the_query_and_count_through() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.search_result.lock() = Some(Ok(vec![forge_primitives::slack::SlackSearchMatch {
            ts: "100.1".to_owned(),
            text: "hello".to_owned(),
            conversation_id: "C1".to_owned(),
            conversation_name: Some("general".to_owned()),
            username: Some("ved".to_owned()),
            user: Some("U9".to_owned()),
            thread_ts: None,
        }]));
        let tool = Search { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({ "query": "hello", "count": 5 }))).await;
        assert!(!out.is_error, "a search succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("general"), "got: {}", out.blocks[0].text);
        assert_eq!(
            mock.search_calls.lock().as_slice(),
            [(None, "hello".to_owned(), 5)],
            "the query and the cap reach the facade",
        );
    }

    #[tokio::test]
    async fn slack_user_passes_the_id_through() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.user_result.lock() = Some(Ok(forge_primitives::slack::SlackUser {
            id: "U1".to_owned(),
            name: "ved".to_owned(),
            real_name: Some("Vedhavyas S".to_owned()),
            tz: Some("Asia/Kolkata".to_owned()),
        }));
        let tool = User { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({ "user": "U1" }))).await;
        assert!(!out.is_error, "a lookup succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("Vedhavyas S"), "got: {}", out.blocks[0].text);
        assert_eq!(mock.user_calls.lock().as_slice(), [(None, "U1".to_owned())]);
    }

    fn pinned_row(ts: &str, text: &str) -> SlackPin {
        SlackPin {
            created: 1_700_000_000,
            created_by: Some("U1".to_owned()),
            message: Some(SlackPinMessage {
                ts: ts.to_owned(),
                user: Some("U2".to_owned()),
                text: text.to_owned(),
            }),
        }
    }

    #[tokio::test]
    async fn slack_pins_passes_the_conversation_through_and_lists_the_rows() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.pins_result.lock() =
            Some(Ok(vec![pinned_row("1700000000.000100", "the pinned text")]));
        let tool = Pins { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(!out.is_error, "a read succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("the pinned text"), "got: {}", out.blocks[0].text);
        assert!(
            out.blocks[0].text.contains("1700000000.000100"),
            "the ts reaches the caller verbatim: {}",
            out.blocks[0].text,
        );
        assert_eq!(
            mock.pins_calls.lock().as_slice(),
            [(None, "C1".to_owned())],
            "the conversation reaches the facade",
        );
    }

    #[tokio::test]
    async fn slack_pins_skips_a_row_without_a_message() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.pins_result.lock() = Some(Ok(vec![
            pinned_row("1700000000.000100", "readable"),
            SlackPin { created: 1, created_by: None, message: None },
        ]));
        let tool = Pins { facade: mock };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(!out.is_error);
        assert!(out.blocks[0].text.contains("readable"), "got: {}", out.blocks[0].text);
        assert_eq!(
            out.blocks[0].text.matches("ts").count(),
            1,
            "a message-less row contributes nothing: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn slack_pins_surfaces_a_facade_error() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.pins_result.lock() = Some(Err(SlackReadError::Fetch("boom".to_owned())));
        let tool = Pins { facade: mock };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(out.is_error, "a failed read is an error, not an empty list");
        assert!(
            out.blocks[0].text.contains("boom"),
            "the cause reaches the LLM: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn slack_bookmarks_lists_the_rows_and_passes_the_conversation_through() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.bookmarks_result.lock() = Some(Ok(vec![SlackBookmark {
            id: "Bk1".to_owned(),
            title: Some("Runbook".to_owned()),
            link: Some("https://example.com".to_owned()),
        }]));
        let tool = Bookmarks { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(!out.is_error, "a read succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("Runbook"), "got: {}", out.blocks[0].text);
        assert_eq!(
            mock.bookmarks_calls.lock().as_slice(),
            [(None, "C1".to_owned())],
            "the conversation reaches the facade",
        );
    }

    #[tokio::test]
    async fn slack_attachment_without_a_file_or_a_path_is_refused() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Attachment { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "workspace": "acme" }))).await;
        assert!(out.is_error, "a call naming neither a file nor a path must not reach Slack");
        assert!(
            out.blocks[0].text.contains("file_id") && out.blocks[0].text.contains("path"),
            "the error says what to pass: {}",
            out.blocks[0].text,
        );
        assert!(mock.fetch_calls.lock().is_empty(), "nothing reaches the facade");
        assert!(mock.upload_calls.lock().is_empty());
    }

    #[tokio::test]
    async fn slack_list_surfaces_a_facade_error() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Err(SlackListError::Fetch("boom".to_owned())));
        let tool = List { facade: mock, caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error, "a failed fetch is an error, not an empty list");
        assert!(
            out.blocks[0].text.contains("boom"),
            "the cause reaches the LLM: {}",
            out.blocks[0].text
        );
    }
}
