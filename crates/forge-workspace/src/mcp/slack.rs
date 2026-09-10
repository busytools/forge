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
    SlackChannelWatch, SlackFacade, SlackListError, SlackSubscribeError, SlackSubscribeRequest,
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
    let unsubscribe = Unsubscribe { facade, caller_key };
    builder.tool(list).tool(subscribe).tool(unsubscribe)
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
}

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "slack__list"
    }

    fn description(&self) -> &'static str {
        "List every conversation in a Slack workspace - public and private channels, DMs and \
         group DMs the token's user is a member of - each row marked with whether YOU are \
         subscribed, counting only your own subscriptions rather than another session's. Pass \
         `workspace` to choose one; omit it when only one is configured. Returns a JSON array \
         of {id, name, kind, subscribed}. Any session in the project may call this."
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
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ListArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let caller = match self.caller_key.current() {
            Ok(key) => key,
            Err(err) => return tool_error(err.to_string()),
        };
        match self.facade.conversations(args.workspace.as_deref()).await {
            Ok(conversations) => {
                let subscribed = self.facade.subscribed_targets(&caller, args.workspace.as_deref());
                let rows = list_rows(&conversations, &subscribed);
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
    conversations: Option<Vec<ChannelWatchArg>>,
}

#[async_trait::async_trait]
impl Tool for Subscribe {
    fn name(&self) -> &'static str {
        "slack__subscribe"
    }

    fn description(&self) -> &'static str {
        "Subscribe YOUR session to a Slack workspace: either the whole DM class, or named \
         conversations with a mode. A conversation in `mentions` mode delivers only messages \
         that mention the user; `all` delivers every message. Pass `workspace` to choose one, \
         or omit it when only one is configured. Records are inert until the poll pump lands; \
         nothing is delivered yet. Returns the new subscription ids. Any session in the project \
         may call this."
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
                "pass `direct_messages: true`, or a non-empty `conversations` array".to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::slack::facade::MockSlackFacade;
    use forge_primitives::slack::SlackConversation;
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
