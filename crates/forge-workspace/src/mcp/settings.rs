//! Settings MCP - forge's configuration, read and edited through the same
//! catalogue and writer the settings page uses (`mcp__forge__settings__*`).
//!
//! The four tools are ANY-CALLER and always on for every session. The
//! config is what the session itself runs under, so no project can
//! withhold the door to it.
//!
//! - `settings__index` - the catalogue's shape: type, state and effect
//!   class per key, no values.
//! - `settings__get` - one key's value; the caller's own project's keys and
//!   the global keys read freely, another project's key is refused by name.
//! - `settings__set` - one catalogue-validated save of one key or a batch
//!   of them, always held for the reader's approval before anything is
//!   written; `settings__unset` clears one key on the same terms.
//!
//! - [`facade`] - the `SettingsFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use crate::SessionSlot;
use crate::mcp::settings::facade::{
    SettingsError, SettingsFacade, SettingsSetItem, SettingsWriteOutcome,
};

pub(crate) mod facade;

/// Attach the settings tools to an existing [`McpServerBuilder`]. Called for
/// BOTH lead and worker sessions, unconditionally: the config is every
/// session's own environment.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn SettingsFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let index = Index { facade: facade.clone(), slot: slot.clone() };
    let get = Get { facade: facade.clone(), slot: slot.clone() };
    let set = Set { facade: facade.clone(), slot: slot.clone() };
    let unset = Unset { facade, slot };
    builder.tool(index).tool(get).tool(set).tool(unset)
}

fn tool_error(error: &SettingsError) -> ToolOutput {
    ToolOutput::error(error.message())
}

fn effect_name(effect: forge_primitives::EffectClass) -> &'static str {
    effect.as_str()
}

struct Index {
    facade: Arc<dyn SettingsFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Index {
    fn name(&self) -> &'static str {
        "settings__index"
    }

    fn description(&self) -> &'static str {
        "Read forge's configuration shape: every catalogue key with its type, whether this \
         config sets it, and when an edit takes effect (`live`, `next session`, `restart`, \
         `not read yet`). No values cross, so this is the shape of the file without its \
         contents. A key with instances lists the ones this config declares; \
         settings__get takes one of those concrete paths (for example \
         `accounts.<display_name>.token`). forge's configuration is read and edited through \
         these settings tools, not by opening or editing a file: a direct filesystem read or \
         edit of the config is the wrong door even when it works, because it skips the \
         catalogue's validation and the reader's approval. Any session may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        match self.facade.index(&self.slot).await {
            Ok(text) => ToolOutput::text(text),
            Err(error) => tool_error(&error),
        }
    }
}

struct Get {
    facade: Arc<dyn SettingsFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct KeyArgs {
    key: String,
}

#[async_trait::async_trait]
impl Tool for Get {
    fn name(&self) -> &'static str {
        "settings__get"
    }

    fn description(&self) -> &'static str {
        "Read one configuration key's effective value. `key` is a concrete catalogue path as \
         settings__index lists it, with the instance filled in where the table repeats \
         (`server.port`, `accounts.<display_name>.token`, \
         `orgs.<org>.projects.<project>.model`). A project's lead and workers read their own \
         project's keys and the global keys freely; another project's key is refused by name, \
         naming the owning project and how to request it - ask that project's lead through the \
         peer channel (`agents__send_message` to that org and project, label \"lead\"), or, \
         for a session that has no peer channel, report the need to your reader. Nothing reads \
         another project's settings silently. forge's configuration is read and edited through \
         these settings tools, not by opening or editing a file: a direct filesystem read or \
         edit of the config is the wrong door even when it works."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The concrete catalogue path, with its instance.",
                },
            },
            "required": ["key"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: KeyArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return ToolOutput::error(format!("invalid arguments: {err}")),
        };
        match self.facade.get(&self.slot, &args.key).await {
            Ok(value) => {
                let body = serde_json::json!({
                    "key": value.key,
                    "pattern": value.pattern,
                    "effect": effect_name(value.effect),
                    "set": value.value != serde_json::Value::Null,
                    "value": value.value,
                });
                match serde_json::to_string_pretty(&body) {
                    Ok(text) => ToolOutput::text(text),
                    Err(err) => ToolOutput::error(format!("response serialization failed: {err}")),
                }
            }
            Err(error) => tool_error(&error),
        }
    }
}

struct Set {
    facade: Arc<dyn SettingsFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct SetArgs {
    edits: Vec<SettingsSetItem>,
}

fn effect_names(effects: &[forge_primitives::EffectClass]) -> String {
    effects.iter().map(|effect| effect_name(*effect)).collect::<Vec<_>>().join(", ")
}

#[async_trait::async_trait]
impl Tool for Set {
    fn name(&self) -> &'static str {
        "settings__set"
    }

    fn description(&self) -> &'static str {
        "Change one or more configuration keys. `edits` is a list of `{\"key\", \"value\"}` \
         objects, each key a concrete catalogue path as settings__index lists it and each \
         value in the key's own type. One call is one validated save, so a section whose keys \
         are mutually required - the vault's `url` and `email`, say - is created by sending \
         both in the same call. The whole batch is validated by the catalogue and the loader \
         first - an unknown key, a wrong type, a save that changes nothing, or a result the \
         loader would refuse is refused naming what failed before anything is asked - and \
         then ALWAYS held for the reader's approval: the write parks in the dock with every \
         key and its current and proposed value, applies through the same validated writer \
         the settings page uses only when they approve, and this call returns which \
         happened. There is no unparked write path, for your own project or any other. \
         forge's configuration is read and edited through these settings tools, not by \
         opening or editing a file: a direct filesystem read or edit of the config is the \
         wrong door even when it works."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The keys and their new values, applied as one validated \
                                    save and one held write. Send a section's mutually \
                                    required keys together.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": {
                                "type": "string",
                                "description": "The concrete catalogue path, with its instance.",
                            },
                            "value": {
                                "description": "The new value in the key's own type (string, \
                                                number, boolean or list of strings). \
                                                settings__index states it.",
                            },
                        },
                        "required": ["key", "value"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["edits"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: SetArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return ToolOutput::error(format!("invalid arguments: {err}")),
        };
        if args.edits.is_empty() {
            return ToolOutput::error("a write needs at least one edit; nothing was written");
        }
        let keys: Vec<String> = args.edits.iter().map(|item| item.key.clone()).collect();
        let named = keys.iter().map(|key| format!("`{key}`")).collect::<Vec<_>>().join(", ");
        match self.facade.set(&self.slot, args.edits).await {
            Ok(SettingsWriteOutcome::Applied { effects }) => {
                if let [key] = keys.as_slice() {
                    ToolOutput::text(format!(
                        "the reader approved; `{key}` was written and takes effect {}",
                        effect_names(&effects),
                    ))
                } else {
                    ToolOutput::text(format!(
                        "the reader approved; {} keys were written and take effect {}: {named}",
                        keys.len(),
                        effect_names(&effects),
                    ))
                }
            }
            Ok(SettingsWriteOutcome::Declined) => {
                if let [key] = keys.as_slice() {
                    ToolOutput::text(format!(
                        "the reader declined; `{key}` is unchanged and nothing was written",
                    ))
                } else {
                    ToolOutput::text(format!(
                        "the reader declined; none of the {} keys changed and nothing was \
                         written: {named}",
                        keys.len(),
                    ))
                }
            }
            Err(error) => tool_error(&error),
        }
    }
}

struct Unset {
    facade: Arc<dyn SettingsFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Unset {
    fn name(&self) -> &'static str {
        "settings__unset"
    }

    fn description(&self) -> &'static str {
        "Clear one optional configuration key, returning it to its default. `key` is a \
         concrete catalogue path as settings__index lists it. A required key is refused by \
         name before anything is asked. Like settings__set the edit is ALWAYS held for the \
         reader's approval: it parks in the dock with the key, its current value and the \
         cleared state, applies through the same validated writer the settings page uses only \
         when they approve, and this call returns which happened. forge's configuration is \
         read and edited through these settings tools, not by opening or editing a file: a \
         direct filesystem read or edit of the config is the wrong door even when it works."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The concrete catalogue path, with its instance.",
                },
            },
            "required": ["key"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: KeyArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return ToolOutput::error(format!("invalid arguments: {err}")),
        };
        match self.facade.unset(&self.slot, &args.key).await {
            Ok(SettingsWriteOutcome::Applied { effects }) => ToolOutput::text(format!(
                "the reader approved; `{}` was cleared and takes effect {}",
                args.key,
                effect_names(&effects),
            )),
            Ok(SettingsWriteOutcome::Declined) => ToolOutput::text(format!(
                "the reader declined; `{}` is unchanged and nothing was written",
                args.key,
            )),
            Err(error) => tool_error(&error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::settings::facade::MockSettingsFacade;
    use forge_sdk::mcp::server::McpServerBuilder;

    /// The one line every settings tool carries: the file is not the door.
    const SHIPPED: &str = "forge's configuration is read and edited through these settings \
                           tools, not by opening or editing a file: a direct filesystem read \
                           or edit of the config is the wrong door even when it works";

    fn slot() -> SessionSlot {
        SessionSlot::lead("Default", "forge")
    }

    fn server(facade: Arc<dyn SettingsFacade>) -> forge_sdk::mcp::server::McpServer {
        add_tools(McpServerBuilder::new("forge", "test"), facade, slot()).build()
    }

    async fn call(
        server: &forge_sdk::mcp::server::McpServer,
        name: &str,
        args: serde_json::Value,
    ) -> (String, bool) {
        let request = forge_sdk::mcp::protocol::JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_owned(),
            params: Some(serde_json::json!({ "name": name, "arguments": args })),
        };
        let response = server.dispatch(&request).await.expect("a tools/call is answered");
        let result = serde_json::to_value(response.result.expect("the call is answered"))
            .expect("serializes");
        let text = result["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (text, is_error)
    }

    /// The tools are the shipped instruction: every one says the config is
    /// read and edited here, not through the filesystem, and none names a
    /// path.
    #[test]
    fn every_tool_says_the_config_is_read_and_edited_here() {
        for description in [
            Index { facade: MockSettingsFacade::new().into_arc(), slot: slot() }.description(),
            Get { facade: MockSettingsFacade::new().into_arc(), slot: slot() }.description(),
            Set { facade: MockSettingsFacade::new().into_arc(), slot: slot() }.description(),
            Unset { facade: MockSettingsFacade::new().into_arc(), slot: slot() }.description(),
        ] {
            assert!(
                description.contains(SHIPPED),
                "the shipped text is missing from a tool description: {description}",
            );
            assert!(
                !description.contains("forge.toml") && !description.contains(".toml"),
                "a tool description advertises the config path: {description}",
            );
        }
    }

    /// The hold is stated before a caller acts, and `__set` says there is no
    /// unparked write path.
    #[test]
    fn the_write_tools_state_the_hold() {
        let set = Set { facade: MockSettingsFacade::new().into_arc(), slot: slot() };
        assert!(set.description().contains("ALWAYS held for the reader's approval"));
        assert!(set.description().contains("no unparked write path"));
        let unset = Unset { facade: MockSettingsFacade::new().into_arc(), slot: slot() };
        assert!(unset.description().contains("ALWAYS held for the reader's approval"));
    }

    /// The cross-project read rule is stated where a reader reaches for it.
    #[test]
    fn get_states_the_cross_project_rule() {
        let get = Get { facade: MockSettingsFacade::new().into_arc(), slot: slot() };
        assert!(get.description().contains("another project's key is refused by name"));
        assert!(get.description().contains("agents__send_message"));
        assert!(get.description().contains("report the need to your reader"));
    }

    #[tokio::test]
    async fn index_returns_the_facades_text_and_takes_no_arguments() {
        let facade = MockSettingsFacade::new();
        *facade.index_text.lock() = "server.port  number  set  restart".to_owned();
        let server = server(facade.into_arc());

        let (text, is_error) = call(&server, "settings__index", serde_json::json!({})).await;

        assert!(!is_error);
        assert!(text.contains("server.port"));
    }

    #[tokio::test]
    async fn get_renders_the_value_with_its_effect() {
        let facade = MockSettingsFacade::new();
        facade.values.lock().insert("server.port".to_owned(), serde_json::json!(8790));
        let server = server(facade.into_arc());

        let (text, is_error) =
            call(&server, "settings__get", serde_json::json!({ "key": "server.port" })).await;

        assert!(!is_error, "{text}");
        assert!(text.contains("8790"), "{text}");
        assert!(text.contains("live"), "{text}");
    }

    #[tokio::test]
    async fn a_refused_read_hands_the_refusal_through() {
        let facade = MockSettingsFacade::new();
        *facade.missing.lock() = Some(SettingsError::ForeignProject {
            key: "orgs.Acme.projects.web.model".to_owned(),
            org: "Acme".to_owned(),
            project: "web".to_owned(),
        });
        let server = server(facade.into_arc());

        let (text, is_error) = call(
            &server,
            "settings__get",
            serde_json::json!({ "key": "orgs.Acme.projects.web.model" }),
        )
        .await;

        assert!(is_error);
        assert!(text.contains("Acme/web"), "the owner is named: {text}");
        assert!(text.contains("agents__send_message"), "the request path is named: {text}");
    }

    /// One call carries the whole batch, the keys arrive in the order the
    /// caller wrote them, and the result says which effects it lands with.
    #[tokio::test]
    async fn set_passes_the_batch_and_says_which_happened() {
        let mock = Arc::new(MockSettingsFacade::new());
        *mock.write_result.lock() = Some(Ok(SettingsWriteOutcome::Applied {
            effects: vec![forge_primitives::EffectClass::Live, forge_primitives::EffectClass::Live],
        }));
        let facade: Arc<dyn SettingsFacade> = mock.clone();
        let server = server(facade);

        let (text, is_error) = call(
            &server,
            "settings__set",
            serde_json::json!({
                "edits": [
                    { "key": "vault.url", "value": "https://vault.test" },
                    { "key": "vault.email", "value": "me@vault.test" },
                ],
            }),
        )
        .await;

        assert!(!is_error, "{text}");
        assert!(text.contains("the reader approved"), "{text}");
        assert!(text.contains("live"), "the effect is stated: {text}");
        let calls = mock.set_calls.lock();
        assert_eq!(calls.len(), 1, "the tool handed the batch through once");
        let (caller, items) = &calls[0];
        assert_eq!(caller, &slot(), "the write is the caller's");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, "vault.url");
        assert_eq!(items[0].value, serde_json::json!("https://vault.test"));
        assert_eq!(items[1].key, "vault.email");
        assert_eq!(items[1].value, serde_json::json!("me@vault.test"));
    }

    /// An empty batch is refused by the tool itself, before the facade: a
    /// save that changes nothing is not a write.
    #[tokio::test]
    async fn set_refuses_an_empty_batch() {
        let mock = Arc::new(MockSettingsFacade::new());
        let facade: Arc<dyn SettingsFacade> = mock.clone();
        let server = server(facade);

        let (text, is_error) =
            call(&server, "settings__set", serde_json::json!({ "edits": [] })).await;

        assert!(is_error, "{text}");
        assert!(text.contains("at least one"), "{text}");
        assert!(mock.set_calls.lock().is_empty(), "nothing reached the facade");
    }

    /// `settings__set` takes one argument - a list of edits - and says why a
    /// batch exists: a section whose keys are mutually required is one save.
    #[test]
    fn set_states_the_batch_form() {
        let set = Set { facade: MockSettingsFacade::new().into_arc(), slot: slot() };
        let schema = set.input_schema();
        assert_eq!(schema["properties"]["edits"]["type"], "array");
        assert!(schema["properties"].get("key").is_none(), "one shape, not two");
        assert_eq!(schema["required"], serde_json::json!(["edits"]));
        let item = &schema["properties"]["edits"]["items"];
        assert_eq!(item["required"], serde_json::json!(["key", "value"]));
        let description = set.description();
        assert!(description.contains("one validated save"), "{description}");
        assert!(description.contains("mutually required"), "{description}");
    }

    #[tokio::test]
    async fn a_declined_write_says_nothing_was_written() {
        let mock = Arc::new(MockSettingsFacade::new());
        let facade: Arc<dyn SettingsFacade> = mock.clone();
        let server = server(facade);

        let (text, is_error) =
            call(&server, "settings__unset", serde_json::json!({ "key": "client.theme" })).await;

        assert!(!is_error, "{text}");
        assert!(text.contains("the reader declined"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
        let calls = mock.unset_calls.lock();
        assert_eq!(calls.len(), 1, "the tool handed the clear through once");
        assert_eq!(calls[0].1, "client.theme");
    }
}
