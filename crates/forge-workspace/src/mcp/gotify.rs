//! Gotify MCP - subscribe to external notifications (`mcp__forge__gotify__*`).
//!
//! A forge session subscribes the configured Gotify server; matching
//! notifications deliver into the session as a user-turn, spawning it if
//! asleep. The tools (`gotify__subscribe` / `gotify__list` /
//! `gotify__unsubscribe`) are ANY-CALLER, scoped to the caller's own
//! project - mirroring the cron family, not the lead-only peers tools.
//!
//! - [`facade`] - the `GotifyFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use forge_primitives::GotifySubscription;
use uuid::Uuid;

use crate::mcp::gotify::facade::{GotifyFacade, GotifySubscribeError};
use crate::mcp::peers::facade::CallerKeyResolver;

pub(crate) mod facade;

/// Attach the three Gotify-coordination tools to an existing
/// [`McpServerBuilder`]. Called for BOTH lead and worker sessions
/// (any-caller), so `build_forge_server` invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn GotifyFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    let subscribe = Subscribe { facade: facade.clone(), caller_key: caller_key.clone() };
    let list = List { facade: facade.clone(), caller_key: caller_key.clone() };
    let unsubscribe = Unsubscribe { facade, caller_key };
    builder.tool(subscribe).tool(list).tool(unsubscribe)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

fn sub_to_json(sub: &GotifySubscription) -> serde_json::Value {
    serde_json::json!({
        "id": sub.id.to_string(),
        "project": sub.project,
        "team_role": sub.team_role,
        "applications": sub.applications,
        "min_priority": sub.min_priority,
    })
}

fn format_subscribe_error(err: &GotifySubscribeError) -> String {
    match err {
        GotifySubscribeError::NotConfigured => {
            "no Gotify server configured in forge.toml [gotify]".to_owned()
        }
        GotifySubscribeError::UnknownCallerProject => {
            "couldn't resolve your project; is this session attached to a forge.toml project?"
                .to_owned()
        }
    }
}

struct Subscribe {
    facade: Arc<dyn GotifyFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct SubscribeArgs {
    #[serde(default)]
    applications: Option<Vec<String>>,
    #[serde(default)]
    min_priority: Option<u8>,
}

#[async_trait::async_trait]
impl Tool for Subscribe {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "gotify__subscribe"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Subscribe YOUR session to the configured Gotify server. When a matching notification \
         arrives it is delivered to you as a user turn (spawning your session if it's asleep). \
         Optionally filter by `applications` (a set of Gotify app NAMEs; a notification matches \
         when its app is any one of them) and/or `min_priority` (only notifications at or above \
         this priority). Both default to any (omit or leave empty). Returns the subscription id \
         (use it with gotify__unsubscribe). Errors if no [gotify] server is configured. Any \
         session in the project may subscribe."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "applications": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Gotify application NAMEs to match; a notification from any \
                                    one of them matches. Omit or leave empty to match any app.",
                },
                "min_priority": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 255,
                    "description": "Only deliver notifications at or above this priority. Omit \
                                    to match any priority.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: SubscribeArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let caller = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        match self.facade.subscribe(&caller, args.applications.unwrap_or_default(), args.min_priority) {
            Ok(id) => ToolOutput::text(format!("subscribed to Gotify (id {id})")),
            Err(err) => tool_error(format_subscribe_error(&err)),
        }
    }
}

struct List {
    facade: Arc<dyn GotifyFacade>,
    caller_key: CallerKeyResolver,
}

#[async_trait::async_trait]
impl Tool for List {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "gotify__list"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "List YOUR project's active Gotify subscriptions. Returns a JSON array of {id, project, \
         team_role, applications, min_priority}. Use an id with gotify__unsubscribe. An empty \
         array means no subscriptions. Takes no arguments. Any session in the project may call \
         this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let caller = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        let subs = self.facade.list(&caller);
        let arr: Vec<serde_json::Value> = subs.iter().map(sub_to_json).collect();
        match serde_json::to_string_pretty(&serde_json::Value::Array(arr)) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("subscription-list serialization failed: {err}")),
        }
    }
}

struct Unsubscribe {
    facade: Arc<dyn GotifyFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct UnsubscribeArgs {
    id: String,
}

#[async_trait::async_trait]
impl Tool for Unsubscribe {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "gotify__unsubscribe"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Remove a Gotify subscription in YOUR project by id (from gotify__list / \
         gotify__subscribe). Any session in the project may call this."
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
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let Ok(id) = Uuid::parse_str(&args.id) else {
            return tool_error(format!("not a valid subscription id: {}", args.id));
        };
        let caller = match self.caller_key.current() {
            Ok(k) => k,
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
    use crate::SessionKey;
    use crate::mcp::gotify::facade::MockGotifyFacade;
    use std::time::SystemTime;

    fn resolver() -> CallerKeyResolver {
        CallerKeyResolver::from_fixed(SessionKey::from_session_id("caller"))
    }

    fn sample_sub(id: Uuid, project: &str) -> GotifySubscription {
        GotifySubscription {
            id,
            project: project.to_owned(),
            team_role: None,
            applications: vec!["alerts".to_owned()],
            min_priority: Some(5),
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput { value }
    }

    #[tokio::test]
    async fn subscribe_calls_facade_and_returns_id() {
        let id = Uuid::from_u128(0x42);
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.subscribe_result.lock() = Some(Ok(id));
        let tool = Subscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool
            .call(input(serde_json::json!({ "applications": ["alerts"], "min_priority": 5 })))
            .await;
        assert!(!out.is_error, "valid subscribe succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains(&id.to_string()), "output carries the id");

        let calls = mock.subscribe_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec!["alerts".to_owned()]);
        assert_eq!(calls[0].2, Some(5));
    }

    #[tokio::test]
    async fn subscribe_surfaces_not_configured_error() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.subscribe_result.lock() = Some(Err(GotifySubscribeError::NotConfigured));
        let tool = Subscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("no Gotify server configured in forge.toml [gotify]"),
            "unconfigured error surfaced to the LLM: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn list_returns_project_subs() {
        let a = Uuid::from_u128(0xa);
        let b = Uuid::from_u128(0xb);
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.subs.lock() = vec![sample_sub(a, "p"), sample_sub(b, "p")];
        let tool = List { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error);
        assert!(
            out.blocks[0].text.contains(&a.to_string())
                && out.blocks[0].text.contains(&b.to_string()),
            "both subscription ids appear in the list output",
        );
    }

    #[tokio::test]
    async fn unsubscribe_removes_by_id() {
        let id = Uuid::from_u128(0xc1);
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.unsubscribe_result.lock() = Some(true);
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "id": id.to_string() }))).await;
        assert!(!out.is_error);
        assert_eq!(mock.unsubscribe_calls.lock()[0].1, id);
    }

    #[tokio::test]
    async fn unsubscribe_missing_id_is_error() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.unsubscribe_result.lock() = Some(false);
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out =
            tool.call(input(serde_json::json!({ "id": Uuid::from_u128(0x9).to_string() }))).await;
        assert!(out.is_error, "removing an unknown id signals an error to the LLM");
    }

    #[tokio::test]
    async fn unsubscribe_rejects_bad_uuid_without_touching_facade() {
        let mock = Arc::new(MockGotifyFacade::new());
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "id": "not-a-uuid" }))).await;
        assert!(out.is_error);
        assert!(mock.unsubscribe_calls.lock().is_empty(), "a bad id never reaches the facade");
    }

    #[test]
    fn tool_names_are_the_gotify_family() {
        let mock = MockGotifyFacade::new().into_arc();
        let resolver = resolver();
        let subscribe = Subscribe { facade: mock.clone(), caller_key: resolver.clone() };
        let list = List { facade: mock.clone(), caller_key: resolver.clone() };
        let unsubscribe = Unsubscribe { facade: mock, caller_key: resolver };
        assert_eq!(subscribe.name(), "gotify__subscribe");
        assert_eq!(list.name(), "gotify__list");
        assert_eq!(unsubscribe.name(), "gotify__unsubscribe");
    }
}
