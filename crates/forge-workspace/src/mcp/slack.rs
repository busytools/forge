//! Slack MCP - read the conversations a configured workspace offers
//! (`mcp__forge__slack__*`).
//!
//! `slack__list` lists every conversation in a workspace and marks which
//! ones are already subscribed. Subscriptions arrive in phase 2, so this
//! phase reports `subscribed: false` for every row and says so in the
//! tool description rather than inventing a store.
//!
//! - [`facade`] - the `SlackFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

pub(crate) mod facade;

use std::sync::Arc;

use forge_primitives::slack::SlackConversation;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::mcp::slack::facade::{SlackFacade, SlackListError};

/// Attach the Slack tools to an existing [`McpServerBuilder`]. Called for
/// BOTH lead and worker sessions (any-caller), so `build_forge_server`
/// invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn SlackFacade>,
) -> McpServerBuilder {
    builder.tool(List { facade })
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

/// One row per conversation, marking the ones in `subscribed`.
fn list_rows(conversations: &[SlackConversation], subscribed: &[String]) -> Vec<serde_json::Value> {
    conversations
        .iter()
        .map(|conversation| {
            serde_json::json!({
                "id": conversation.id,
                "name": conversation_name(conversation),
                "kind": conversation_kind(conversation),
                "subscribed": subscribed.iter().any(|id| id == &conversation.id),
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
         group DMs the token's user is a member of - each row marked with whether it is already \
         subscribed. Pass `workspace` to choose one; omit it when only one is configured. \
         Subscriptions are not implemented yet, so every row reports `subscribed: false`. \
         Returns a JSON array of {id, name, kind, subscribed}. Any session in the project may \
         call this."
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
        match self.facade.conversations(args.workspace.as_deref()).await {
            Ok(conversations) => {
                let rows = list_rows(&conversations, &[]);
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

    #[test]
    fn list_marks_a_conversation_as_subscribed_only_when_it_is() {
        let conversations = vec![channel("C1", "general")];
        let rows = list_rows(&conversations, &["C1".to_owned()]);
        assert_eq!(rows.len(), 1, "one row per conversation");
        assert_eq!(rows[0]["id"], "C1");
        assert_eq!(rows[0]["name"], "general");
        assert_eq!(rows[0]["subscribed"], true);

        let rows = list_rows(&conversations, &[]);
        assert_eq!(rows[0]["subscribed"], false, "an empty subscribed set marks nothing");
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
        let tool = List { facade: mock.clone() };

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

    #[tokio::test]
    async fn slack_list_surfaces_a_facade_error() {
        let mock = Arc::new(MockSlackFacade::new());
        *mock.conversations_result.lock() = Some(Err(SlackListError::Fetch("boom".to_owned())));
        let tool = List { facade: mock };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error, "a failed fetch is an error, not an empty list");
        assert!(
            out.blocks[0].text.contains("boom"),
            "the cause reaches the LLM: {}",
            out.blocks[0].text
        );
    }
}
