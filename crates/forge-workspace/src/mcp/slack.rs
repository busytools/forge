//! Slack MCP - read the conversations the caller's workspace offers
//! (`mcp__forge__slack__*`).
//!
//! No tool takes a `workspace` argument: the calling session's slot names
//! its project, and `forge.toml`'s org or project binding names that
//! project's workspace. `slack__list` lists every conversation in it and
//! marks which ones the caller has subscribed; `slack__subscribe` and
//! `slack__unsubscribe` manage those records. A subscription is live: the
//! per-workspace poll pump sweeps it and delivers matches into the
//! owning session.
//!
//! - [`facade`] - the `SlackFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

pub(crate) mod facade;

use std::sync::Arc;

use forge_connectors::slack::SlackHistoryMessage;
use forge_primitives::slack::{
    SlackConversation, SlackSubscription, SlackSubscriptionTarget, SlackWatchMode,
};
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};
use uuid::Uuid;

use crate::SessionSlot;
use crate::mcp::slack::facade::{
    SlackAttachmentError, SlackChannelWatch, SlackEditError, SlackEditRequest, SlackFacade,
    SlackFetchRequest, SlackPostError, SlackPostRequest, SlackReactError, SlackReactRequest,
    SlackReadError, SlackSubscribeError, SlackSubscribeRequest, SlackUnsubscribeError,
    SlackUploadRequest, format_workspace_error,
};

/// Attach the Slack tools to an existing [`McpServerBuilder`]. Called for
/// BOTH lead and worker sessions (any-caller), so `build_forge_server`
/// invokes this unconditionally. `subscribe` / `unsubscribe` are scoped
/// to the caller's own records, so they need the caller slot.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let list = List { facade: facade.clone(), slot: slot.clone() };
    let subscribe = Subscribe { facade: facade.clone(), slot: slot.clone() };
    let unsubscribe = Unsubscribe { facade: facade.clone(), slot: slot.clone() };
    let post = Post { facade: facade.clone(), slot: slot.clone() };
    let edit = Edit { facade: facade.clone(), slot: slot.clone() };
    let react = React { facade: facade.clone(), slot: slot.clone() };
    let attachment = Attachment { facade: facade.clone(), slot: slot.clone() };
    let search = Search { facade: facade.clone(), slot: slot.clone() };
    let history = History { facade: facade.clone(), slot: slot.clone() };
    let replies = Replies { facade: facade.clone(), slot: slot.clone() };
    let user = User { facade: facade.clone(), slot: slot.clone() };
    let users = Users { facade: facade.clone(), slot: slot.clone() };
    let me = Me { facade: facade.clone(), slot: slot.clone() };
    let pins = Pins { facade: facade.clone(), slot: slot.clone() };
    let bookmarks = Bookmarks { facade, slot };
    builder
        .tool(list)
        .tool(subscribe)
        .tool(unsubscribe)
        .tool(post)
        .tool(edit)
        .tool(react)
        .tool(attachment)
        .tool(search)
        .tool(history)
        .tool(replies)
        .tool(user)
        .tool(users)
        .tool(me)
        .tool(pins)
        .tool(bookmarks)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput::error(text)
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

/// The caller's subscription ids that cover `conversation` - after a
/// restart these are the only handle a session has to unsubscribe with.
fn covering_subscription_ids(
    conversation: &SlackConversation,
    subscribed: &[SlackSubscription],
) -> Vec<Uuid> {
    subscribed
        .iter()
        .filter(|sub| match &sub.target {
            SlackSubscriptionTarget::DirectMessages => conversation.is_im || conversation.is_mpim,
            SlackSubscriptionTarget::Conversation { id, .. } => *id == conversation.id,
            // A mention target is not a conversation subscription, so it
            // marks no row.
            SlackSubscriptionTarget::Mentions => false,
        })
        .map(|sub| sub.id)
        .collect()
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

/// One row per conversation, marking the ones in `subscribed` and
/// listing the caller's own subscription ids that cover it.
fn list_rows(
    conversations: &[SlackConversation],
    subscribed: &[SlackSubscriptionTarget],
    caller_subscriptions: &[SlackSubscription],
) -> Vec<serde_json::Value> {
    conversations
        .iter()
        .map(|conversation| {
            serde_json::json!({
                "id": conversation.id,
                "name": conversation_name(conversation),
                "kind": conversation_kind(conversation),
                "subscribed": is_subscribed(conversation, subscribed),
                "subscription_ids": covering_subscription_ids(conversation, caller_subscriptions)
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The caller's own class subscriptions - the DM class and the mention
/// target. Neither covers one conversation, so no row can carry them, and
/// their ids are the only handle `slack__unsubscribe` accepts. The target
/// names match the ones the Inspector's SLACK section shows.
fn class_subscriptions(subscribed: &[SlackSubscription]) -> Vec<serde_json::Value> {
    subscribed
        .iter()
        .filter_map(|sub| {
            let target = match &sub.target {
                SlackSubscriptionTarget::DirectMessages => "direct messages",
                SlackSubscriptionTarget::Mentions => "mentions anywhere",
                SlackSubscriptionTarget::Conversation { .. } => return None,
            };
            Some(serde_json::json!({
                "id": sub.id.to_string(),
                "workspace": sub.workspace,
                "target": target,
            }))
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

struct List {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ListArgs {
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
        "List every conversation in the Slack workspace bound to your project - public and \
         private channels, DMs and group DMs the token's user is a member of - each row marked \
         with whether YOU are subscribed, counting only your own subscriptions rather than \
         another session's. Pass `name` for only conversations whose name, purpose or topic \
         contains that text (case-insensitive), or `kind` for only one conversation type. \
         Returns a JSON object of three keys. `workspace` is the resolved `[[slack]]` label; \
         `conversations` is an array of {id, name, kind, subscribed, subscription_ids}; \
         `subscription_ids` lists your own ids covering that conversation, which is what \
         slack__unsubscribe takes. `subscriptions` lists your class subscriptions - the DM \
         class, and the workspace mention target with its id - since neither covers one \
         conversation, so no row can carry them. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        match self.facade.conversations(&self.slot).await {
            Ok((workspace, conversations)) => {
                let own = self.facade.subscribed_targets(&self.slot, &workspace);
                let subscribed: Vec<SlackSubscriptionTarget> =
                    own.iter().map(|sub| sub.target.clone()).collect();
                let kept: Vec<SlackConversation> = conversations
                    .iter()
                    .filter(|conversation| {
                        passes_filter(conversation, args.name.as_deref(), args.kind.as_deref())
                    })
                    .cloned()
                    .collect();
                let rows = list_rows(&kept, &subscribed, &own);
                let listed = serde_json::json!({
                    "workspace": workspace,
                    "conversations": rows,
                    "subscriptions": class_subscriptions(&own),
                });
                match serde_json::to_string_pretty(&listed) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => {
                        tool_error(format!("conversation-list serialization failed: {err}"))
                    }
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

/// One subscription as a tool-result row: its id (the only handle
/// `slack__unsubscribe` accepts), the workspace it watches, and its target
/// in a shape a reader can switch on.
fn sub_to_row(sub: &SlackSubscription) -> serde_json::Value {
    serde_json::json!({
        "id": sub.id.to_string(),
        "workspace": sub.workspace,
        "target": target_to_json(&sub.target),
    })
}

/// The subscription target's tool-output spelling. The durable enum's own
/// serde form is externally tagged and unreadable (`"DirectMessages"`, or
/// `{"Conversation": {..}}`), so results carry this instead; `name` is
/// absent until the sweep's directory walk has seen the conversation.
fn target_to_json(target: &SlackSubscriptionTarget) -> serde_json::Value {
    match target {
        SlackSubscriptionTarget::DirectMessages => serde_json::json!({ "kind": "dm" }),
        SlackSubscriptionTarget::Mentions => serde_json::json!({ "kind": "mentions" }),
        SlackSubscriptionTarget::Conversation { id, name, mode } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".to_owned(), serde_json::json!("conversation"));
            map.insert("id".to_owned(), serde_json::json!(id));
            if let Some(name) = name {
                map.insert("name".to_owned(), serde_json::json!(name));
            }
            map.insert(
                "mode".to_owned(),
                serde_json::json!(match mode {
                    SlackWatchMode::All => "all",
                    SlackWatchMode::MentionsOnly => "mentions",
                }),
            );
            serde_json::Value::Object(map)
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

fn format_subscribe_error(err: &SlackSubscribeError) -> String {
    match err {
        SlackSubscribeError::Workspace(err) => format_workspace_error(err),
        SlackSubscribeError::UnknownCallerProject => {
            "couldn't resolve your project; is this session attached to a forge.toml project?"
                .to_owned()
        }
    }
}

struct Subscribe {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct SubscribeArgs {
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
         delivers only messages that mention the user; `all` delivers every message. Returns \
         one row per subscription as {id, workspace, target}, where target is \
         {kind: \"conversation\", \
         id, name, mode} | {kind: \"dm\"} | {kind: \"mentions\"}; the id is the only handle \
         slack__unsubscribe accepts. A conversation already watched hands back that record \
         rather than creating a second one. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for request in requests {
            match self.facade.subscribe(&self.slot, request) {
                Ok(created) => rows.extend(created.iter().map(sub_to_row)),
                Err(err) => {
                    return tool_error(format_subscribe_error(&err));
                }
            }
        }
        match serde_json::to_string_pretty(&serde_json::Value::Array(rows)) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("response serialization failed: {err}")),
        }
    }
}

struct Unsubscribe {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
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
         Returns the removed subscription row. Any session in the project may call this."
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
        match self.facade.unsubscribe(&self.slot, id) {
            Ok(removed) => {
                let envelope = crate::mcp::deleted::removed_record(&sub_to_row(&removed));
                match serde_json::to_string_pretty(&envelope) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(SlackUnsubscribeError::UnknownCallerProject) => {
                tool_error("couldn't resolve your project".to_owned())
            }
            Err(SlackUnsubscribeError::NoSuchSubscription) => {
                tool_error(format!("no subscription with id {id} in this project"))
            }
            Err(SlackUnsubscribeError::NotOwnedByCaller) => tool_error(format!(
                "subscription {id} exists in this project but was not subscribed by you"
            )),
        }
    }
}

fn format_post_error(err: &SlackPostError) -> String {
    match err {
        SlackPostError::Rejected => "the post was not approved, so nothing was sent".to_owned(),
        SlackPostError::Expired => {
            "the draft expired unanswered, so nothing was sent; call again when the user can \
             decide"
                .to_owned()
        }
        SlackPostError::Workspace(err) => format_workspace_error(err),
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
        SlackEditError::Expired => {
            "the draft expired unanswered, so the message is untouched; call again when the user \
             can decide"
                .to_owned()
        }
        SlackEditError::Workspace(err) => format_workspace_error(err),
        SlackEditError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

fn format_react_error(err: &SlackReactError) -> String {
    match err {
        SlackReactError::Rejected => {
            "the reaction was not approved, so nothing was added or removed".to_owned()
        }
        SlackReactError::Expired => {
            "the draft expired unanswered, so nothing was added or removed; call again when the \
             user can decide"
                .to_owned()
        }
        SlackReactError::Workspace(err) => format_workspace_error(err),
        SlackReactError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

struct Post {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct PostArgs {
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
         nothing. Text past 4000 characters is split into numbered parts automatically, each a \
         separate message. Returns {workspace, ts, conversation_name, parts}: `workspace` is \
         the resolved `[[slack]]` label it posted into, and `ts` names every message it posted, \
         in the order they went out; slack__edit, slack__react and a reply each take one. Any \
         session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        let request = SlackPostRequest {
            conversation: args.conversation,
            thread_ts: args.thread_ts,
            text: args.text,
        };
        match self.facade.post(&self.slot, request).await {
            Ok(outcome) => match serde_json::to_string_pretty(&serde_json::json!({
                "workspace": outcome.workspace,
                "ts": outcome.ts,
                "conversation_name": outcome.conversation_name,
                "parts": outcome.parts,
            })) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_post_error(&err)),
        }
    }
}

struct Edit {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct EditArgs {
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
         message untouched. Slack refuses a message you did not post - including an edit of \
         your own thread reply, which a channel-history lookup cannot see. Any session in \
         the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        let request = SlackEditRequest {
            conversation: args.conversation,
            ts: args.ts,
            text: if delete { None } else { args.text },
        };
        match self.facade.edit(&self.slot, request).await {
            Ok(()) => {
                ToolOutput::text(if delete { "deleted".to_owned() } else { "updated".to_owned() })
            }
            Err(err) => tool_error(format_edit_error(&err)),
        }
    }
}

struct React {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ReactArgs {
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
         reaction's author. HELD FOR APPROVAL like slack__post: a reaction is authored content \
         in the user's name. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        let request = SlackReactRequest {
            conversation: args.conversation,
            ts: args.ts,
            name: args.name,
            add: !args.remove.unwrap_or(false),
        };
        match self.facade.react(&self.slot, request).await {
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
        SlackAttachmentError::Expired => {
            "the draft expired unanswered, so nothing was sent; call again when the user can \
             decide"
                .to_owned()
        }
        SlackAttachmentError::Workspace(err) => format_workspace_error(err),
        SlackAttachmentError::Fetch(message) => format!("Slack request failed: {message}"),
        SlackAttachmentError::NotADirectory(path) => {
            format!("`dir` is a file, not a directory: {}", path.display())
        }
    }
}

struct Attachment {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct AttachmentArgs {
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
                let request = SlackFetchRequest { file_id, dir: std::path::PathBuf::from(dir) };
                match self.facade.fetch_attachment(&self.slot, request).await {
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
                let request = SlackUploadRequest {
                    conversation,
                    thread_ts: args.thread_ts,
                    path: std::path::PathBuf::from(path),
                    title: None,
                };
                match self.facade.post_attachment(&self.slot, request).await {
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
        SlackReadError::Workspace(err) => format_workspace_error(err),
        SlackReadError::Fetch(message) => format!("Slack request failed: {message}"),
    }
}

/// Results per `slack__search` when the caller does not cap them.
const DEFAULT_SEARCH_COUNT: u32 = 20;

struct Search {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    conversation: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Search {
    fn name(&self) -> &'static str {
        "slack__search"
    }

    fn description(&self) -> &'static str {
        "Search the Slack workspace bound to your project by text, newest first. This is a \
         LOOKUP, not a mention detector: it matches the words you pass and cannot find every \
         message that mentions a user - subscribe to the mention target for that. Pass \
         `conversation` with an id from slack__list to keep only that conversation's hits. \
         Returns a JSON object: `workspace` is the resolved `[[slack]]` label, and `matches` is \
         an array of {ts, text, conversation, conversation_name, username}; `conversation_name` \
         is null for a DM, which has none. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
                "conversation": {
                    "type": "string",
                    "description": "Only hits from this conversation id, as slack__list \
                                    reports it.",
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
        match self.facade.search(&self.slot, &args.query, count, args.conversation.as_deref()).await
        {
            Ok((workspace, found)) => {
                let rows: Vec<serde_json::Value> = found
                    .iter()
                    .map(|hit| {
                        serde_json::json!({
                            "ts": hit.ts,
                            "text": hit.text,
                            "conversation": hit.conversation_id,
                            "conversation_name": hit.conversation_name,
                            "username": hit.username,
                            "files": hit.files.iter().map(|file| serde_json::json!({
                                "id": file.id,
                                "name": file.name,
                            })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                let found = serde_json::json!({ "workspace": workspace, "matches": rows });
                match serde_json::to_string_pretty(&found) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("search serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

/// Messages per read when the caller does not cap them.
const DEFAULT_HISTORY_LIMIT: u32 = 50;

/// Slack's own ceiling for one `conversations.history` page.
const MAX_HISTORY_LIMIT: u32 = 999;

/// Replies per thread read when the caller does not cap them.
const DEFAULT_REPLIES_LIMIT: u32 = 200;

/// Slack's own ceiling for one `conversations.replies` page.
const MAX_REPLIES_LIMIT: u32 = 1000;

/// One message as both read tools report it. `files` keeps the ids an
/// attachment fetch takes; the file's bearer URL never leaves the connector.
fn message_row(message: &SlackHistoryMessage) -> serde_json::Value {
    serde_json::json!({
        "ts": message.ts,
        "user": message.user,
        "bot_name": message.bot_name,
        "text": message.text,
        "thread_ts": message.thread_ts,
        "reply_count": message.reply_count,
        "edited": message.edited,
        "files": message.files.iter().map(|file| serde_json::json!({
            "id": file.id,
            "name": file.name,
        })).collect::<Vec<_>>(),
    })
}

fn read_rows(messages: &[SlackHistoryMessage]) -> serde_json::Value {
    serde_json::Value::Array(messages.iter().map(message_row).collect())
}

struct History {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct HistoryArgs {
    conversation: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    oldest: Option<String>,
    #[serde(default)]
    newest: Option<String>,
}

#[async_trait::async_trait]
impl Tool for History {
    fn name(&self) -> &'static str {
        "slack__history"
    }

    fn description(&self) -> &'static str {
        "Read one Slack conversation's messages, newest first. `oldest` and `newest` are Slack \
         ts bounds and both are exclusive; to read further back, pass the earliest `ts` of the \
         page you just read as `newest`. Returns a JSON object: `workspace` is the resolved \
         `[[slack]]` label, and `messages` is an array of {ts, user, bot_name, text, thread_ts, \
         reply_count, edited, files}; `user` is the author's id, which slack__user resolves to a \
         name, and `reply_count` above zero means slack__replies has more. Read-only and not \
         held for approval. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "conversation": {
                    "type": "string",
                    "description": "The conversation id from slack__list.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_HISTORY_LIMIT,
                    "description": "Maximum messages to read (default 50).",
                },
                "oldest": {
                    "type": "string",
                    "description": "Only messages newer than this Slack ts.",
                },
                "newest": {
                    "type": "string",
                    "description": "Only messages older than this Slack ts.",
                },
            },
            "required": ["conversation"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: HistoryArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let limit = args.limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
        if limit == 0 || limit > MAX_HISTORY_LIMIT {
            return tool_error(format!(
                "`limit` must be between 1 and {MAX_HISTORY_LIMIT}, Slack's page ceiling"
            ));
        }
        match self
            .facade
            .history(
                &self.slot,
                &args.conversation,
                limit,
                args.oldest.as_deref(),
                args.newest.as_deref(),
            )
            .await
        {
            Ok((workspace, messages)) => {
                let page = serde_json::json!({
                    "workspace": workspace,
                    "messages": read_rows(&messages),
                });
                match serde_json::to_string_pretty(&page) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("history serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct Replies {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct RepliesArgs {
    conversation: String,
    ts: String,
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait::async_trait]
impl Tool for Replies {
    fn name(&self) -> &'static str {
        "slack__replies"
    }

    fn description(&self) -> &'static str {
        "Read one Slack thread: the parent message first, then replies oldest first, up to \
         `limit` rows (default 200, max 1000). `conversation` is the id from slack__list and \
         `ts` the parent message's ts, which is the `ts` slack__history reports for it. Returns \
         the same shape as slack__history - `workspace` and `messages` - with the parent exactly \
         once. Read-only and not held for approval. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "conversation": {
                    "type": "string",
                    "description": "The conversation id from slack__list.",
                },
                "ts": {
                    "type": "string",
                    "description": "The parent message's ts.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_REPLIES_LIMIT,
                    "description": "Maximum messages to read (default 200).",
                },
            },
            "required": ["conversation", "ts"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: RepliesArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let limit = args.limit.unwrap_or(DEFAULT_REPLIES_LIMIT);
        if limit == 0 || limit > MAX_REPLIES_LIMIT {
            return tool_error(format!(
                "`limit` must be between 1 and {MAX_REPLIES_LIMIT}, Slack's page ceiling"
            ));
        }
        match self.facade.replies(&self.slot, &args.conversation, &args.ts, limit).await {
            Ok((workspace, messages)) => {
                let thread = serde_json::json!({
                    "workspace": workspace,
                    "messages": read_rows(&messages),
                });
                match serde_json::to_string_pretty(&thread) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("replies serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct User {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct UserArgs {
    user: String,
}

#[async_trait::async_trait]
impl Tool for User {
    fn name(&self) -> &'static str {
        "slack__user"
    }

    fn description(&self) -> &'static str {
        "Look up one Slack user by id: their handle, real name and timezone. Returns \
         {id, name, real_name, tz} plus `workspace`, the resolved `[[slack]]` label; each of \
         real_name and tz is null when the workspace does not report it. Use slack__users to \
         find an id from a name. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        match self.facade.user(&self.slot, &args.user).await {
            Ok((workspace, user)) => {
                let body = serde_json::json!({
                    "workspace": workspace,
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

struct Me {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct MeArgs {}

#[async_trait::async_trait]
impl Tool for Me {
    fn name(&self) -> &'static str {
        "slack__me"
    }

    fn description(&self) -> &'static str {
        "Report which Slack identity the project's workspace token acts as, over `auth.test`: \
         the team and the user behind it. `user_id` is what a message's `user` field carries \
         and what a \
         mention is written with, so this is how a caller knows which id is its own. Returns \
         {workspace, team, team_id, user, user_id, url}; `workspace` is the `[[slack]]` label, \
         which can differ from the team's name. Read-only and not held for approval. Any \
         session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let MeArgs {} = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.me(&self.slot).await {
            Ok((workspace, auth)) => {
                let body = serde_json::json!({
                    "workspace": workspace,
                    "team": auth.team,
                    "team_id": auth.team_id,
                    "user": auth.user,
                    "user_id": auth.user_id,
                    "url": auth.url,
                });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("identity serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct Users {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct UsersArgs {
    query: String,
}

#[async_trait::async_trait]
impl Tool for Users {
    fn name(&self) -> &'static str {
        "slack__users"
    }

    fn description(&self) -> &'static str {
        "Find Slack users by handle, real name or display name, so a mention can name the right \
         id. Matching is a case-insensitive substring over all three, and EVERY match is \
         returned - silently choosing one of several is how a mention reaches the wrong person, \
         so read the rows and pick the `id` yourself. Write a mention as `<@ID>`. Deactivated \
         members are excluded. Returns a JSON object: `workspace` is the resolved `[[slack]]` \
         label, and `users` is an array of {id, name, real_name, display_name}. Read-only and \
         not held for approval. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A handle, real name or display name, or part of one.",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: UsersArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        if args.query.trim().is_empty() {
            return tool_error(
                "pass a handle, real name or display name to search for; an empty query would \
                 return the whole workspace"
                    .to_owned(),
            );
        }
        match self.facade.users(&self.slot, &args.query).await {
            Ok((workspace, found)) => {
                let rows: Vec<serde_json::Value> = found
                    .iter()
                    .map(|user| {
                        serde_json::json!({
                            "id": user.id,
                            "name": user.name,
                            "real_name": user.real_name,
                            "display_name": user
                                .profile
                                .as_ref()
                                .and_then(|profile| profile.display_name.as_deref()),
                        })
                    })
                    .collect();
                let found = serde_json::json!({ "workspace": workspace, "users": rows });
                match serde_json::to_string_pretty(&found) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("user-list serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct Pins {
    facade: Arc<dyn SlackFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct PinsArgs {
    conversation: String,
}

#[async_trait::async_trait]
impl Tool for Pins {
    fn name(&self) -> &'static str {
        "slack__pins"
    }

    fn description(&self) -> &'static str {
        "List a Slack conversation's pinned messages - what the channel has kept as standing \
         context. Read-only and not held for approval. Returns a JSON object: `workspace` is \
         the resolved `[[slack]]` label, and `pins` is an array of {ts, user, text}. Any \
         session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        match self.facade.pins(&self.slot, &args.conversation).await {
            Ok((workspace, pins)) => {
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
                let listed = serde_json::json!({ "workspace": workspace, "pins": rows });
                match serde_json::to_string_pretty(&listed) {
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
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct BookmarksArgs {
    conversation: String,
}

#[async_trait::async_trait]
impl Tool for Bookmarks {
    fn name(&self) -> &'static str {
        "slack__bookmarks"
    }

    fn description(&self) -> &'static str {
        "List a Slack conversation's bookmarks - the links saved on the channel. Read-only and \
         not held for approval. Returns a JSON object: `workspace` is the resolved `[[slack]]` \
         label, and `bookmarks` is an array of {id, title, link}, each of the last two null when \
         the workspace does not report it. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
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
        match self.facade.bookmarks(&self.slot, &args.conversation).await {
            Ok((workspace, bookmarks)) => {
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
                let listed = serde_json::json!({ "workspace": workspace, "bookmarks": rows });
                match serde_json::to_string_pretty(&listed) {
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
    use crate::mcp::slack::facade::{MockSlackFacade, SlackPostOutcome, SlackWorkspaceError};
    use crate::mcp::test_support::text_of;
    use forge_connectors::slack::AuthTest;
    use forge_primitives::slack::{
        SlackBookmark, SlackConversation, SlackConversationText, SlackPin, SlackPinMessage,
        SlackUser, SlackUserProfile,
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
        SlackSubscriptionTarget::Conversation {
            id: id.to_owned(),
            name: None,
            mode: SlackWatchMode::All,
        }
    }

    fn owned_sub(
        id: Uuid,
        target: SlackSubscriptionTarget,
    ) -> forge_primitives::slack::SlackSubscription {
        forge_primitives::slack::SlackSubscription {
            id,
            workspace: "acme".to_owned(),
            project: "forge".to_owned(),
            team_role: None,
            target,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// A mention target covers no conversation, so it appears on no row. It
    /// is still live and swept, and without its id nothing can remove it.
    #[tokio::test]
    async fn slack_list_reports_class_subscriptions_with_their_ids() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Ok(vec![channel("C1", "general")]));
        let mentions = owned_sub(Uuid::from_u128(0x77), SlackSubscriptionTarget::Mentions);
        *mock.subscribed_targets.lock() = vec![mentions.clone()];
        let tool = List { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;

        assert!(!out.is_error, "the list succeeds: {}", text_of(&out));
        assert!(
            text_of(&out).contains(&mentions.id.to_string()),
            "the class target's id is reachable: {}",
            text_of(&out),
        );
        assert!(
            text_of(&out).contains("mentions anywhere"),
            "and it is named as the target it is: {}",
            text_of(&out),
        );
    }

    /// One structured row per subscription, compared whole against the
    /// row formatter so no key can be dropped or renamed: the ids are the
    /// only handle `slack__unsubscribe` accepts, and the target is what
    /// says a row is a channel, the DM class or a workspace-wide mention.
    #[tokio::test]
    async fn slack_subscribe_returns_one_row_per_created_subscription() {
        let mock = Arc::new(MockSlackFacade::new());
        let dm = owned_sub(Uuid::from_u128(0xabc), SlackSubscriptionTarget::DirectMessages);
        let channel = owned_sub(
            Uuid::from_u128(0xdef),
            SlackSubscriptionTarget::Conversation {
                id: "C1".to_owned(),
                name: Some("general".to_owned()),
                mode: SlackWatchMode::MentionsOnly,
            },
        );
        let mentions = owned_sub(Uuid::from_u128(0x1a), SlackSubscriptionTarget::Mentions);
        *mock.subscribe_result.lock() =
            Some(Ok(vec![dm.clone(), channel.clone(), mentions.clone()]));
        let tool = Subscribe { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "direct_messages": true }))).await;

        assert!(!out.is_error, "a valid subscribe succeeds: {}", text_of(&out));
        assert_eq!(out.blocks.len(), 1, "the rows stay one text block: {out:?}");
        let rows: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is one row per record");
        assert_eq!(rows[0], sub_to_row(&dm), "the DM class's row, whole");
        assert_eq!(rows[1], sub_to_row(&channel), "the conversation's row, name and mode included");
        assert_eq!(
            rows[2],
            sub_to_row(&mentions),
            "and the mention target's own spelling, which the description promises",
        );
        assert_eq!(rows[0]["id"], dm.id.to_string(), "the id is the unsubscribe handle");
    }

    /// One call naming two kinds keeps both kinds' rows: collecting the
    /// last request's rows instead of extending would drop the DM class's,
    /// and with them its only unsubscribe handle.
    #[tokio::test]
    async fn slack_subscribe_keeps_every_kinds_rows() {
        let mock = Arc::new(MockSlackFacade::new());
        // The mock answers every request with this row, so the count is
        // what discriminates: two requests, both answers kept.
        *mock.subscribe_result.lock() = Some(Ok(vec![owned_sub(
            Uuid::from_u128(0xabc),
            SlackSubscriptionTarget::DirectMessages,
        )]));
        let tool = Subscribe { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "direct_messages": true,
                "mentions": true,
            })))
            .await;

        assert!(!out.is_error, "a two-kind subscribe succeeds: {}", text_of(&out));
        assert_eq!(mock.subscribe_calls.lock().len(), 2, "both kinds reach the facade");
        let rows: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is one row per record");
        assert_eq!(rows.as_array().expect("a row list").len(), 2, "and both answers are kept");
    }

    /// "Nothing new to watch" is an empty array, not a sentence: a reader
    /// parses one shape, and prose turns a no-op answer into an unreadable
    /// one.
    #[tokio::test]
    async fn slack_subscribe_with_nothing_new_still_answers_a_list() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.subscribe_result.lock() = Some(Ok(Vec::new()));
        let tool = Subscribe { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "mentions": true }))).await;

        assert!(!out.is_error, "nothing new to watch is not an error: {}", text_of(&out));
        let rows: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the answer is a row list");
        assert_eq!(rows, serde_json::json!([]), "an empty list, not prose");
    }

    /// Editing, deleting or reacting all take the ts, and Slack has
    /// already sent it: a count is not a handle. The conversation's name
    /// and the part count come along so a reader can name the channel
    /// without a second lookup and see that the text was split.
    #[tokio::test]
    async fn slack_post_reports_the_ts_the_channel_and_the_part_count() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.post_result.lock() = Some(Ok(SlackPostOutcome {
            workspace: "acme".to_owned(),
            ts: vec!["1790186552.442169".to_owned(), "1790186552.442170".to_owned()],
            conversation_name: "granite-staging".to_owned(),
            parts: 2,
        }));
        let tool = Post { facade: mock, slot: caller_slot() };

        let out =
            tool.call(input(serde_json::json!({ "conversation": "C1", "text": "hello" }))).await;

        assert!(!out.is_error, "the post succeeds: {}", text_of(&out));
        assert_eq!(out.blocks.len(), 1, "the record stays one text block: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the posted record");
        assert_eq!(
            json["ts"],
            serde_json::json!(["1790186552.442169", "1790186552.442170"]),
            "the ts of every message, in the order they went out",
        );
        assert_eq!(json["conversation_name"], "granite-staging");
        assert_eq!(json["parts"], 2, "so a reader sees the text went out in two messages");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
    }

    #[test]
    fn list_marks_a_conversation_as_subscribed_only_when_it_is() {
        let conversations = vec![channel("C1", "general")];
        let rows = list_rows(&conversations, &[watching("C1")], &[]);
        assert_eq!(rows.len(), 1, "one row per conversation");
        assert_eq!(rows[0]["id"], "C1");
        assert_eq!(rows[0]["name"], "general");
        assert_eq!(rows[0]["subscribed"], true);

        let rows = list_rows(&conversations, &[], &[]);
        assert_eq!(rows[0]["subscribed"], false, "an empty subscribed set marks nothing");
    }

    #[test]
    fn a_watched_conversation_is_marked_and_an_unwatched_one_is_not() {
        let conversations = vec![channel("C1", "general"), channel("C2", "random")];
        let rows = list_rows(&conversations, &[watching("C1")], &[]);
        let by_id =
            |id: &str| rows.iter().find(|row| row["id"] == id).expect("row for the id").clone();
        assert_eq!(by_id("C1")["subscribed"], true, "the watched channel is marked");
        assert_eq!(by_id("C2")["subscribed"], false, "its unwatched sibling is not");
    }

    #[test]
    fn the_dm_class_marks_every_dm_and_group_dm() {
        let conversations = vec![dm("D1", "U1"), dm("D2", "U2"), channel("C1", "general")];
        let rows = list_rows(&conversations, &[SlackSubscriptionTarget::DirectMessages], &[]);
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
        let rows = list_rows(&[dm("D1", "U9")], &[], &[]);
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

        let rows =
            list_rows(&[channel("C1", "general"), dm("D1", "U9"), private, group_dm], &[], &[]);
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
        assert!(passes_filter(&ch, Some("RELEASE"), None), "the topic matches, case-insensitively");
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
    fn each_workspace_refusal_names_its_cause() {
        let not_configured = format_workspace_error(&SlackWorkspaceError::NotConfigured);
        assert!(not_configured.contains("[[slack]]"), "got: {not_configured}");

        let unbound = format_workspace_error(&SlackWorkspaceError::Unbound {
            org: "Granite".to_owned(),
            project: "core-v1".to_owned(),
        });
        assert!(unbound.contains("Granite/core-v1"), "names the scope: {unbound}");
        assert!(unbound.contains("[orgs.slack]"), "names the org key: {unbound}");
        assert!(unbound.contains("[orgs.projects.slack]"), "names the project key: {unbound}");

        let not_loaded = format_workspace_error(&SlackWorkspaceError::BoundButNotLive {
            label: "acme".to_owned(),
        });
        assert!(not_loaded.contains("'acme'"), "names the label: {not_loaded}");
        assert!(not_loaded.contains("restart"), "names the way out: {not_loaded}");

        let fetch = format_read_error(&SlackReadError::Fetch("boom".to_owned()));
        assert!(fetch.contains("boom"), "carries the cause: {fetch}");
    }

    /// A project with no binding has one answer whichever tool was called:
    /// a tool that reworded it would send the caller looking for a config key
    /// that is already set.
    #[test]
    fn every_tool_words_a_workspace_refusal_the_same_way() {
        let unbound = SlackWorkspaceError::Unbound {
            org: "Granite".to_owned(),
            project: "core-v1".to_owned(),
        };
        let expected = format_workspace_error(&unbound);
        let answers = [
            format_read_error(&SlackReadError::Workspace(unbound.clone())),
            format_post_error(&SlackPostError::Workspace(unbound.clone())),
            format_edit_error(&SlackEditError::Workspace(unbound.clone())),
            format_react_error(&SlackReactError::Workspace(unbound.clone())),
            format_attachment_error(&SlackAttachmentError::Workspace(unbound.clone())),
            format_subscribe_error(&SlackSubscribeError::Workspace(unbound)),
        ];
        for answer in answers {
            assert_eq!(answer, expected, "every tool shares the one wording");
        }
    }

    /// The headline of the cut: no Slack tool advertises a `workspace`
    /// argument, whatever its verb. One re-advertised property is a model
    /// that keeps passing an argument the tool no longer reads.
    #[test]
    fn no_slack_tool_advertises_a_workspace_argument() {
        let facade: Arc<dyn SlackFacade> = Arc::new(MockSlackFacade::new());
        let server =
            add_tools(McpServerBuilder::new("forge", "test"), facade, caller_slot()).build();
        let schemas = server.tool_schemas();
        assert_eq!(schemas.len(), 15, "every Slack tool is registered: {schemas:?}");
        for (name, schema) in &schemas {
            assert!(
                schema["properties"].get("workspace").is_none(),
                "{name} still advertises a workspace argument: {schema}",
            );
        }
    }

    #[tokio::test]
    async fn slack_list_returns_rows_and_asks_for_the_callers_slot() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Ok(vec![channel("C1", "general")]));
        let tool = List { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "a configured workspace succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("C1"), "the row reaches the output: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the list object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        assert_eq!(
            mock.conversations_calls.lock().as_slice(),
            [caller_slot()],
            "the tool hands the facade the calling session's slot, never a workspace argument",
        );
        assert_eq!(
            mock.subscribed_targets_calls.lock().as_slice(),
            [(caller_slot(), "acme".to_owned())],
            "the marks use the read's own resolved label, not a second resolution",
        );
    }

    /// A caller that still sends `workspace` is ignored, not refused: the
    /// argument is gone from the schema, and a stale transcript replaying it
    /// must not fail the call.
    #[tokio::test]
    async fn a_stale_workspace_argument_is_ignored() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Ok(vec![channel("C1", "general")]));
        let tool = List { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "workspace": "elsewhere" }))).await;
        assert!(!out.is_error, "the stale argument does not refuse the call: {}", text_of(&out));
        assert_eq!(mock.conversations_calls.lock().as_slice(), [caller_slot()]);
    }

    fn caller_slot() -> SessionSlot {
        SessionSlot::from_str_for_test("caller")
    }

    #[tokio::test]
    async fn slack_subscribe_passes_the_targets_through_to_the_facade() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.subscribe_result.lock() =
            Some(Ok(vec![owned_sub(Uuid::from_u128(0x42), SlackSubscriptionTarget::Mentions)]));
        let tool = Subscribe { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "conversations": [{ "id": "C1", "mode": "mentions" }],
            })))
            .await;
        assert!(!out.is_error, "a valid subscribe succeeds: {}", text_of(&out));

        let calls = mock.subscribe_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, caller_slot(), "the caller's slot is what the facade resolves on");
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
        let tool = Subscribe { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error, "a call naming nothing to watch must not create records");
        assert!(
            text_of(&out).contains("direct_messages"),
            "the error says what to pass: {}",
            text_of(&out),
        );
        assert!(mock.subscribe_calls.lock().is_empty(), "nothing reaches the facade");
    }

    #[tokio::test]
    async fn slack_subscribe_names_the_unbound_scope() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.subscribe_result.lock() =
            Some(Err(SlackSubscribeError::Workspace(SlackWorkspaceError::Unbound {
                org: "Granite".to_owned(),
                project: "core-v1".to_owned(),
            })));
        let tool = Subscribe { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "direct_messages": true }))).await;
        assert!(out.is_error);
        assert!(
            text_of(&out).contains("Granite/core-v1"),
            "the error names the scope that has no binding: {}",
            text_of(&out),
        );
    }

    /// A refusal has to say which case it is: an id that exists in the
    /// project but is not the caller's own reads as *not yours*, an id
    /// the project never had reads as *not there*. One string for both
    /// lets a caller take its own refused unsubscribe as proof the
    /// subscription is gone.
    #[tokio::test]
    async fn slack_unsubscribe_refusals_name_the_case() {
        let id = Uuid::from_u128(0xc1);
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Unsubscribe { facade: mock.clone(), slot: caller_slot() };

        *mock.unsubscribe_result.lock() = Some(Err(SlackUnsubscribeError::NotOwnedByCaller));
        let not_yours = tool.call(input(serde_json::json!({ "id": id.to_string() }))).await;
        assert!(not_yours.is_error, "a refused unsubscribe is an error");
        assert_eq!(
            text_of(&not_yours),
            format!("subscription {id} exists in this project but was not subscribed by you"),
            "a subscription that exists but is not the caller's own must say so",
        );

        *mock.unsubscribe_result.lock() = Some(Err(SlackUnsubscribeError::NoSuchSubscription));
        let not_there = tool.call(input(serde_json::json!({ "id": id.to_string() }))).await;
        assert!(not_there.is_error, "an unknown id is an error");
        assert_eq!(
            text_of(&not_there),
            format!("no subscription with id {id} in this project"),
            "an id the project never had must not read as another session's subscription",
        );
    }

    /// An unsubscribe echoes the row it removed, whole, so a reader can
    /// name the target that stopped; a bare uuid names nothing a human can
    /// place. The target here carries no name, which is also the case that
    /// pins the key as absent rather than null.
    #[tokio::test]
    async fn slack_unsubscribe_echoes_the_removed_row() {
        let mock = Arc::new(MockSlackFacade::new());
        let removed = owned_sub(
            Uuid::from_u128(0x9),
            SlackSubscriptionTarget::Conversation {
                id: "C1".to_owned(),
                name: None,
                mode: SlackWatchMode::All,
            },
        );
        *mock.unsubscribe_result.lock() = Some(Ok(removed.clone()));
        let tool = Unsubscribe { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "id": removed.id.to_string() }))).await;
        assert!(!out.is_error, "unsubscribe succeeds: {out:?}");
        assert_eq!(out.blocks.len(), 1, "the envelope stays one text block: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the adopted envelope");
        assert_eq!(json["status"], "deleted");
        assert_eq!(
            json["removed"],
            sub_to_row(&removed),
            "the echo is the whole row, and an unknown name stays absent: {json}",
        );
    }

    #[tokio::test]
    async fn slack_unsubscribe_rejects_a_bad_uuid_without_touching_the_facade() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Unsubscribe { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "id": "not-a-uuid" }))).await;
        assert!(out.is_error);
        assert!(mock.unsubscribe_calls.lock().is_empty(), "a bad id never reaches the facade");
    }

    #[tokio::test]
    async fn slack_list_marks_what_the_caller_watches() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() =
            Some(Ok(vec![channel("C1", "general"), channel("C2", "random")]));
        *mock.subscribed_targets.lock() = vec![owned_sub(Uuid::from_u128(0x21), watching("C1"))];
        let tool = List { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {}", text_of(&out));
        let listed: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the output is JSON");
        let by_id = |id: &str| {
            listed["conversations"]
                .as_array()
                .expect("an array of rows")
                .iter()
                .find(|row| row["id"] == id)
                .expect("row for the id")
                .clone()
        };
        assert_eq!(by_id("C1")["subscribed"], true, "the caller's target reaches its row");
        assert_eq!(by_id("C2")["subscribed"], false, "an unwatched row stays unmarked");
        assert_eq!(
            by_id("C1")["subscription_ids"].as_array().expect("the ids ride the watched row").len(),
            1,
            "the caller's own subscription id reaches the row",
        );
        assert!(
            by_id("C2")["subscription_ids"].as_array().expect("ids ride every row").is_empty(),
            "an unwatched row carries no subscription ids",
        );
    }

    #[tokio::test]
    async fn slack_list_filters_apply_before_the_rows_are_built() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() =
            Some(Ok(vec![channel("C1", "general"), dm("D1", "U9")]));
        let tool = List { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "kind": "im" }))).await;
        assert!(!out.is_error, "a known kind succeeds: {}", text_of(&out));
        let listed: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the output is JSON");
        let ids: Vec<&str> = listed["conversations"]
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
        let tool = List { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "kind": "chanels" }))).await;
        assert!(out.is_error, "a typo must not read as an empty workspace");
        assert!(
            text_of(&out).contains("chanels"),
            "the error names what was rejected: {}",
            text_of(&out),
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
            files: Vec::new(),
        }]));
        let tool = Search { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "query": "hello",
                "count": 5,
                "conversation": "C1",
            })))
            .await;
        assert!(!out.is_error, "a search succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("general"), "got: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the search object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        assert_eq!(json["matches"][0]["text"], "hello", "the hits ride under their own key");
        assert_eq!(
            mock.search_calls.lock().as_slice(),
            [(caller_slot(), "hello".to_owned(), 5, Some("C1".to_owned()))],
            "the caller's slot, the query, the cap and the conversation filter reach the facade",
        );
    }

    fn history_message(ts: &str, text: &str) -> SlackHistoryMessage {
        SlackHistoryMessage {
            ts: ts.to_owned(),
            user: Some("U1".to_owned()),
            text: text.to_owned(),
            thread_ts: None,
            reply_count: 0,
            bot_name: None,
            parent_user_id: None,
            latest_reply: None,
            edited: None,
            files: Vec::new(),
        }
    }

    /// The bounds are what make the read a window rather than the channel's
    /// newest page forever: both must reach Slack, and the author's id must
    /// reach the caller so a name can be resolved.
    #[tokio::test]
    async fn slack_history_passes_the_bounds_and_reports_the_rows() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.history_result.lock() = Some(Ok(vec![history_message("100.5", "hello")]));
        let tool = History { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "conversation": "C1",
                "limit": 5,
                "oldest": "100.0",
                "newest": "200.0",
            })))
            .await;

        assert!(!out.is_error, "a read succeeds: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the history object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        let rows = &json["messages"];
        assert_eq!(rows[0]["ts"], "100.5", "the ts the caller pages with");
        assert_eq!(rows[0]["user"], "U1", "the author id a name resolves from");
        assert_eq!(rows[0]["text"], "hello");
        assert_eq!(
            mock.history_calls.lock().as_slice(),
            [(
                caller_slot(),
                "C1".to_owned(),
                5,
                Some("100.0".to_owned()),
                Some("200.0".to_owned())
            )],
            "both bounds, the limit and the conversation reach the facade",
        );
    }

    #[tokio::test]
    async fn slack_history_refuses_a_limit_past_slacks_ceiling() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = History { facade: mock.clone(), slot: caller_slot() };

        let out =
            tool.call(input(serde_json::json!({ "conversation": "C1", "limit": 1000 }))).await;
        assert!(out.is_error, "a page past Slack's ceiling cannot be served");
        assert!(text_of(&out).contains("999"), "the error names the ceiling: {}", text_of(&out));
        assert!(mock.history_calls.lock().is_empty(), "nothing reaches the facade");
    }

    /// A thread read is the whole thread: the parent first and then the
    /// replies, which is what the read path was missing.
    #[tokio::test]
    async fn slack_replies_passes_the_parent_and_reports_the_thread() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.replies_result.lock() =
            Some(Ok(vec![history_message("100.0", "parent"), history_message("100.5", "reply")]));
        let tool = Replies { facade: mock.clone(), slot: caller_slot() };

        let out =
            tool.call(input(serde_json::json!({ "conversation": "C1", "ts": "100.0" }))).await;

        assert!(!out.is_error, "a thread read succeeds: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the replies object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        let rows = &json["messages"];
        assert_eq!(rows[0]["text"], "parent", "the parent comes first");
        assert_eq!(rows[1]["text"], "reply");
        assert_eq!(
            mock.replies_calls.lock().as_slice(),
            [(caller_slot(), "C1".to_owned(), "100.0".to_owned(), DEFAULT_REPLIES_LIMIT)],
            "the conversation, parent ts and default cap reach the facade",
        );
    }

    #[tokio::test]
    async fn slack_replies_refuses_a_limit_past_slacks_ceiling() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Replies { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "conversation": "C1",
                "ts": "100.0",
                "limit": 1001,
            })))
            .await;

        assert!(out.is_error, "a read past Slack's ceiling cannot be served");
        assert!(text_of(&out).contains("1000"), "the error names the ceiling: {}", text_of(&out));
        assert!(mock.replies_calls.lock().is_empty(), "nothing reaches the facade");
    }

    /// A limit the caller passes must be the one the facade is given: a tool
    /// that always used the default would otherwise pass every other test.
    #[tokio::test]
    async fn slack_replies_forwards_the_callers_limit() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Replies { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "conversation": "C1",
                "ts": "100.0",
                "limit": 50,
            })))
            .await;

        assert!(!out.is_error, "a read succeeds: {}", text_of(&out));
        assert_eq!(
            mock.replies_calls.lock().as_slice(),
            [(caller_slot(), "C1".to_owned(), "100.0".to_owned(), 50)],
            "the caller's limit reaches the facade",
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
            deleted: false,
            profile: None,
        }));
        let tool = User { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "user": "U1" }))).await;
        assert!(!out.is_error, "a lookup succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("Vedhavyas S"), "got: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the user record");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        assert_eq!(mock.user_calls.lock().as_slice(), [(caller_slot(), "U1".to_owned())]);
    }

    fn resolved_user(id: &str, name: &str, display_name: Option<&str>) -> SlackUser {
        SlackUser {
            id: id.to_owned(),
            name: name.to_owned(),
            real_name: None,
            tz: None,
            deleted: false,
            profile: display_name.map(|display_name| SlackUserProfile {
                display_name: Some(display_name.to_owned()),
            }),
        }
    }

    /// The resolver hands back every match so the caller picks the id, and
    /// the display name rides the row: it is often what the caller typed.
    #[tokio::test]
    async fn slack_users_returns_every_match_with_its_id() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.users_result.lock() = Some(Ok(vec![
            resolved_user("U1", "sam", Some("Sam")),
            resolved_user("U2", "samuel", None),
        ]));
        let tool = Users { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "query": "sam" }))).await;

        assert!(!out.is_error, "a lookup succeeds: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the users object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        let rows = &json["users"];
        assert_eq!(rows[0]["id"], "U1");
        assert_eq!(rows[1]["id"], "U2", "both matches are returned, none silently chosen");
        assert_eq!(rows[0]["display_name"], "Sam");
        assert_eq!(
            mock.users_calls.lock().as_slice(),
            [(caller_slot(), "sam".to_owned())],
            "the query reaches the facade",
        );
    }

    #[tokio::test]
    async fn slack_users_refuses_an_empty_query() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Users { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "query": "   " }))).await;

        assert!(out.is_error, "an empty query would return the whole workspace");
        assert!(mock.users_calls.lock().is_empty(), "nothing reaches the facade");
    }

    /// The `[[slack]]` label and the team's own name are different things
    /// (`Subspace` is team `Autonomys`), so both have to reach the caller.
    #[tokio::test]
    async fn slack_me_reports_the_label_and_the_token_identity() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.me_result.lock() = Some(Ok((
            "Subspace".to_owned(),
            AuthTest {
                team: "Autonomys".to_owned(),
                user: "ved".to_owned(),
                team_id: "T2".to_owned(),
                user_id: "U2".to_owned(),
                url: "https://autonomys.slack.com/".to_owned(),
            },
        )));
        let tool = Me { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;

        assert!(!out.is_error, "an identity lookup succeeds: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the identity record");
        assert_eq!(json["workspace"], "Subspace", "the resolved label is named in the result");
        assert_eq!(json["team"], "Autonomys", "the team the token belongs to");
        assert_eq!(json["user"], "ved");
        assert_eq!(json["user_id"], "U2", "the id a mention and a message's user carry");
        assert_eq!(json["url"], "https://autonomys.slack.com/");
        assert_eq!(
            mock.me_calls.lock().as_slice(),
            [caller_slot()],
            "the caller's slot is what the facade resolves on",
        );
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
        let tool = Pins { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(!out.is_error, "a read succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("the pinned text"), "got: {}", text_of(&out));
        assert!(
            text_of(&out).contains("1700000000.000100"),
            "the ts reaches the caller verbatim: {}",
            text_of(&out),
        );
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the pins object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        assert_eq!(
            mock.pins_calls.lock().as_slice(),
            [(caller_slot(), "C1".to_owned())],
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
        let tool = Pins { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(!out.is_error);
        assert!(text_of(&out).contains("readable"), "got: {}", text_of(&out));
        assert_eq!(
            text_of(&out).matches("ts").count(),
            1,
            "a message-less row contributes nothing: {}",
            text_of(&out),
        );
    }

    #[tokio::test]
    async fn slack_pins_surfaces_a_facade_error() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.pins_result.lock() = Some(Err(SlackReadError::Fetch("boom".to_owned())));
        let tool = Pins { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(out.is_error, "a failed read is an error, not an empty list");
        assert!(text_of(&out).contains("boom"), "the cause reaches the LLM: {}", text_of(&out));
    }

    #[tokio::test]
    async fn slack_bookmarks_lists_the_rows_and_passes_the_conversation_through() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.bookmarks_result.lock() = Some(Ok(vec![SlackBookmark {
            id: "Bk1".to_owned(),
            title: Some("Runbook".to_owned()),
            link: Some("https://example.com".to_owned()),
        }]));
        let tool = Bookmarks { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "conversation": "C1" }))).await;
        assert!(!out.is_error, "a read succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("Runbook"), "got: {}", text_of(&out));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the bookmarks object");
        assert_eq!(json["workspace"], "acme", "the resolved label rides the result");
        assert_eq!(
            mock.bookmarks_calls.lock().as_slice(),
            [(caller_slot(), "C1".to_owned())],
            "the conversation reaches the facade",
        );
    }

    #[tokio::test]
    async fn slack_attachment_without_a_file_or_a_path_is_refused() {
        let mock = Arc::new(MockSlackFacade::new());
        let tool = Attachment { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error, "a call naming neither a file nor a path must not reach Slack");
        assert!(
            text_of(&out).contains("file_id") && text_of(&out).contains("path"),
            "the error says what to pass: {}",
            text_of(&out),
        );
        assert!(mock.fetch_calls.lock().is_empty(), "nothing reaches the facade");
        assert!(mock.upload_calls.lock().is_empty());
    }

    #[tokio::test]
    async fn slack_attachment_names_the_directory_when_it_is_a_file() {
        let mock = Arc::new(MockSlackFacade::new());
        let offender = std::env::temp_dir().join("slack-attachment-not-a-dir");
        *mock.fetch_result.lock() =
            Some(Err(SlackAttachmentError::NotADirectory(offender.clone())));
        let tool = Attachment { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "file_id": "F1",
                "dir": offender.display().to_string(),
            })))
            .await;
        assert!(out.is_error, "a file in the dir slot is an error: {}", text_of(&out));
        assert!(
            text_of(&out).contains(&offender.display().to_string()),
            "the rendered error names the path the caller chose: {}",
            text_of(&out),
        );
    }

    #[tokio::test]
    async fn slack_list_surfaces_a_facade_error() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Err(SlackReadError::Fetch("boom".to_owned())));
        let tool = List { facade: mock, slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error, "a failed fetch is an error, not an empty list");
        assert!(text_of(&out).contains("boom"), "the cause reaches the LLM: {}", text_of(&out));
    }
}
