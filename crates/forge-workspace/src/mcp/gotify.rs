//! Gotify MCP - subscribe to + read from external notifications
//! (`mcp__forge__gotify__*`).
//!
//! A forge session subscribes the configured Gotify server; matching
//! notifications deliver into the session as a user-turn, spawning it if
//! asleep. `gotify__subscribe` / `gotify__list` / `gotify__unsubscribe`
//! manage the caller's OWN subscriptions - a lead the lead's, a worker
//! its own, neither the other's; `gotify__apps` / `gotify__recent` are
//! read-only server queries (application names, recent-notification
//! catch-up). All are ANY-CALLER - mirroring the cron family, not the
//! lead-only peers tools, down to the owner scoping on list + remove.
//!
//! - [`facade`] - the `GotifyFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use forge_connectors::gotify::GotifyRecent;
use forge_primitives::GotifySubscription;
use uuid::Uuid;

use crate::mcp::gotify::facade::{GotifyFacade, GotifyReadError, GotifySubscribeError};
use crate::mcp::peers::facade::CallerKeyResolver;

pub(crate) mod facade;
pub mod types;

/// Attach the five Gotify tools to an existing [`McpServerBuilder`].
/// Called for BOTH lead and worker sessions (any-caller), so
/// `build_forge_server` invokes this unconditionally. `apps` / `recent`
/// are server-global reads and don't take the caller key.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn GotifyFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    let subscribe = Subscribe { facade: facade.clone(), caller_key: caller_key.clone() };
    let list = List { facade: facade.clone(), caller_key: caller_key.clone() };
    let apps = Apps { facade: facade.clone() };
    let recent = Recent { facade: facade.clone() };
    let unsubscribe = Unsubscribe { facade, caller_key };
    builder.tool(subscribe).tool(list).tool(unsubscribe).tool(apps).tool(recent)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

/// Readable JSON for one subscription (the tool-output shape the LLM
/// sees). No `team_role`: `gotify__list` only ever returns the caller's
/// own, so the field could carry just one value, and `cron_to_json`
/// omits it for the same reason.
fn sub_to_json(sub: &GotifySubscription) -> serde_json::Value {
    serde_json::json!({
        "id": sub.id.to_string(),
        "project": sub.project,
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

/// Default notification count for `gotify__recent` when `limit` is omitted.
const DEFAULT_RECENT_LIMIT: usize = 20;

fn format_read_error(err: &GotifyReadError) -> String {
    match err {
        GotifyReadError::NotConfigured => {
            "no Gotify server configured in forge.toml [gotify]".to_owned()
        }
        GotifyReadError::Fetch(msg) => format!("Gotify request failed: {msg}"),
    }
}

fn recent_to_json(n: &GotifyRecent) -> serde_json::Value {
    serde_json::json!({
        "app": n.app,
        "title": n.title,
        "message": n.message,
        "priority": n.priority,
        "date": n.date,
    })
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
    fn name(&self) -> &'static str {
        "gotify__subscribe"
    }

    fn description(&self) -> &'static str {
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
        match self.facade.subscribe(
            &caller,
            args.applications.unwrap_or_default(),
            args.min_priority,
        ) {
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
    fn name(&self) -> &'static str {
        "gotify__list"
    }

    fn description(&self) -> &'static str {
        "List YOUR OWN active Gotify subscriptions (a lead sees the lead's, a worker its own). \
         Returns a JSON array of {id, project, applications, min_priority}. Use an id with \
         gotify__unsubscribe. An empty array means no subscriptions. Takes no arguments. Any \
         session in the project may call this."
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
    fn name(&self) -> &'static str {
        "gotify__unsubscribe"
    }

    fn description(&self) -> &'static str {
        "Remove one of YOUR OWN Gotify subscriptions by id (from gotify__list / \
         gotify__subscribe), scoped to what you subscribed - a caller manages only what it \
         created. Any session in the project may call this."
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

struct Apps {
    facade: Arc<dyn GotifyFacade>,
}

#[async_trait::async_trait]
impl Tool for Apps {
    fn name(&self) -> &'static str {
        "gotify__apps"
    }

    fn description(&self) -> &'static str {
        "List the application NAMEs on the configured Gotify server. Use these names to filter \
         gotify__subscribe / gotify__recent by app. Returns a JSON array of strings. Takes no \
         arguments. Errors if no [gotify] server is configured."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        match self.facade.apps().await {
            Ok(names) => match serde_json::to_string_pretty(&names) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("application-list serialization failed: {err}")),
            },
            Err(err) => tool_error(format_read_error(&err)),
        }
    }
}

struct Recent {
    facade: Arc<dyn GotifyFacade>,
}

#[derive(serde::Deserialize)]
struct RecentArgs {
    #[serde(default)]
    applications: Option<Vec<String>>,
    #[serde(default)]
    min_priority: Option<u8>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for Recent {
    fn name(&self) -> &'static str {
        "gotify__recent"
    }

    fn description(&self) -> &'static str {
        "Fetch the most recent notifications from the configured Gotify server, newest first - a \
         catch-up for a session that was asleep or just woke. Optionally filter by `applications` \
         (a set of Gotify app NAMEs, from gotify__apps) and/or `min_priority` (at or above), and \
         cap the count with `limit` (default 20). Returns a JSON array of {app, title, message, \
         priority, date}. Errors if no [gotify] server is configured."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "applications": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Gotify application NAMEs to include; a notification from any \
                                    one of them matches. Omit or leave empty to include any app.",
                },
                "min_priority": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 255,
                    "description": "Only include notifications at or above this priority. Omit \
                                    to include any priority.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of notifications to return (default 20).",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: RecentArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let limit = args.limit.unwrap_or(DEFAULT_RECENT_LIMIT);
        match self
            .facade
            .recent(args.applications.unwrap_or_default(), args.min_priority, limit)
            .await
        {
            Ok(items) => {
                let arr: Vec<serde_json::Value> = items.iter().map(recent_to_json).collect();
                match serde_json::to_string_pretty(&serde_json::Value::Array(arr)) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => {
                        tool_error(format!("recent-notifications serialization failed: {err}"))
                    }
                }
            }
            Err(err) => tool_error(format_read_error(&err)),
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
    async fn list_returns_the_callers_own_subs() {
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

    /// The facade reports `false` both for an unknown id and for one
    /// owned by another session, so reaching for someone else's
    /// subscription gets exactly what a bad id gets - the same treatment
    /// `cron__delete` gives.
    #[tokio::test]
    async fn unsubscribe_unowned_or_missing_id_is_error() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.unsubscribe_result.lock() = Some(false);
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out =
            tool.call(input(serde_json::json!({ "id": Uuid::from_u128(0x9).to_string() }))).await;
        assert!(out.is_error, "an unremovable id signals an error to the LLM");
    }

    #[tokio::test]
    async fn unsubscribe_rejects_bad_uuid_without_touching_facade() {
        let mock = Arc::new(MockGotifyFacade::new());
        let tool = Unsubscribe { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "id": "not-a-uuid" }))).await;
        assert!(out.is_error);
        assert!(mock.unsubscribe_calls.lock().is_empty(), "a bad id never reaches the facade");
    }

    fn recent(app: &str, title: &str, priority: u8) -> GotifyRecent {
        GotifyRecent {
            app: app.to_owned(),
            title: title.to_owned(),
            message: "body".to_owned(),
            priority,
            date: "2026-07-04T09:18:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn apps_returns_names_from_facade() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.apps_result.lock() = Some(Ok(vec!["Backups".to_owned(), "CI".to_owned()]));
        let tool = Apps { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "apps succeeds: {}", out.blocks[0].text);
        assert!(
            out.blocks[0].text.contains("Backups") && out.blocks[0].text.contains("CI"),
            "both app names appear: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn apps_surfaces_not_configured_error() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.apps_result.lock() = Some(Err(GotifyReadError::NotConfigured));
        let tool = Apps { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("no Gotify server configured in forge.toml [gotify]"),
            "unconfigured error surfaced: {}",
            out.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn recent_serializes_notifications_and_defaults_the_limit() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.recent_result.lock() = Some(Ok(vec![recent("CI", "build failed", 8)]));
        let tool = Recent { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "recent succeeds: {}", out.blocks[0].text);
        assert!(
            out.blocks[0].text.contains("CI") && out.blocks[0].text.contains("build failed"),
            "notification fields serialized: {}",
            out.blocks[0].text,
        );
        let calls = mock.recent_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, 20, "an omitted limit defaults to 20");
    }

    #[tokio::test]
    async fn recent_passes_explicit_filters_and_limit_to_facade() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.recent_result.lock() = Some(Ok(vec![]));
        let tool = Recent { facade: mock.clone() };

        let out = tool
            .call(input(
                serde_json::json!({ "applications": ["CI"], "min_priority": 5, "limit": 3 }),
            ))
            .await;
        assert!(!out.is_error);
        let calls = mock.recent_calls.lock();
        assert_eq!(calls[0].0, vec!["CI".to_owned()]);
        assert_eq!(calls[0].1, Some(5));
        assert_eq!(calls[0].2, 3);
    }

    #[tokio::test]
    async fn recent_surfaces_fetch_error() {
        let mock = Arc::new(MockGotifyFacade::new());
        *mock.recent_result.lock() = Some(Err(GotifyReadError::Fetch("boom".to_owned())));
        let tool = Recent { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("boom"),
            "fetch failure surfaced to the LLM: {}",
            out.blocks[0].text,
        );
    }

    #[test]
    fn tool_names_are_the_gotify_family() {
        let mock = MockGotifyFacade::new().into_arc();
        let resolver = resolver();
        let subscribe = Subscribe { facade: mock.clone(), caller_key: resolver.clone() };
        let list = List { facade: mock.clone(), caller_key: resolver.clone() };
        let unsubscribe = Unsubscribe { facade: mock.clone(), caller_key: resolver };
        let apps = Apps { facade: mock.clone() };
        let recent = Recent { facade: mock };
        assert_eq!(subscribe.name(), "gotify__subscribe");
        assert_eq!(list.name(), "gotify__list");
        assert_eq!(unsubscribe.name(), "gotify__unsubscribe");
        assert_eq!(apps.name(), "gotify__apps");
        assert_eq!(recent.name(), "gotify__recent");
    }
}
