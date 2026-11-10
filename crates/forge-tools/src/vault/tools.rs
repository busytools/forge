//! The vault family's MCP tools (`mcp__forge__vault__*`).
//!
//! Thirteen tools, every one held for the reader's approval: the blocked
//! handler parks a `VaultDraft` in the family's registry, the dock draws it,
//! and no operation runs in the vault unless the answer approved it. The card
//! never carries a secret value; a reveal happens only after approval and
//! travels in the tool result.
//!
//! The family describes itself through `forge_mcp::ToolProvider`, so the
//! host registers it without this crate depending on the CLI subprocess
//! crate: the names, descriptions and schemas here are what the host
//! advertises, unchanged from when the host owned them.

use std::sync::Arc;

use forge_mcp::{Tool, ToolOutput, ToolProvider};
use forge_primitives::SessionSlot;
use forge_primitives::vault::VaultTool;
use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::vault::facade::{CreateInput, PatchInput, VaultFacade, VaultToolError};

/// The vault family, bound to the session whose dock answers its calls.
pub struct VaultTools {
    facade: Arc<dyn VaultFacade>,
    slot: SessionSlot,
}

impl VaultTools {
    pub fn new(facade: Arc<dyn VaultFacade>, slot: SessionSlot) -> Self {
        Self { facade, slot }
    }

    fn describe(tool: VaultTool) -> &'static str {
        match tool {
            VaultTool::Search => {
                "Search the vault by item name, username or URI, case-insensitively. Returns \
                 metadata only: {items: [{id, name, username, uris, folder, collection, \
                 updated_at}]}. Passwords, notes, TOTP seeds and card numbers are never \
                 decrypted by a search. The call is held for the reader's approval, and the \
                 card shows the query and the hit count."
            }
            VaultTool::Get => {
                "Read one vault item. By default returns metadata only: id, name, type, \
                 username, uris, folder_id, card brand, attachments and dates. Pass `reveal` to \
                 return one selected value: password, notes, custom:<name>, card.cardholder, \
                 card.number, card.expiry (an object of expMonth and expYear) or card.code. \
                 The TOTP seed is never a reveal option; use vault__totp_code. The call is \
                 held; the card names the item and the field and shows no value."
            }
            VaultTool::Create => {
                "Create a login, secure note or card. `type` defaults to login. A login sets \
                 username, password, uri and totp (an otpauth:// URI or the bare base32 seed); \
                 a card sets cardholder_name, number, exp_month, exp_year, code and optionally \
                 brand; a note uses notes. Pass `custom_fields` as [{name, value, type?}] with \
                 type text, hidden or boolean. The call is held; the card says what is being \
                 created and masks every secret value."
            }
            VaultTool::Update => {
                "Change fields on one item. `patch` accepts name, username, password, uri, \
                 totp, notes, cardholder_name, number, exp_month, exp_year, code, brand, \
                 folder_id and custom_fields; only the fields present are changed. The call is \
                 held; the card names the item and a masked diff, never the values."
            }
            VaultTool::Trash => {
                "Move one item to the trash (a soft delete). The call is held; the card names \
                 the item. Use vault__restore to bring it back."
            }
            VaultTool::Restore => {
                "Restore one item from the trash. The call is held; the card names the item."
            }
            VaultTool::TotpCode => {
                "Compute the current TOTP code for one item, in process. Returns {code, \
                 period, remaining}; the seed never leaves the process and is never returned. \
                 The call is held; the card names the item."
            }
            VaultTool::AttachmentGet => {
                "Download and decrypt one attachment to `destination`, which must resolve \
                 inside this session's working directory. Returns {path, size, sha256}; the \
                 file is written mode 0600. The call is held; the card names the item, the \
                 attachment and its size."
            }
            VaultTool::AttachmentPut => {
                "Upload one file from `path`, which must resolve inside this session's working \
                 directory. Pass `replace_attachment_id` to replace an existing attachment as \
                 one held intent naming both files; the delete is never a separate tool. \
                 Returns {id, name, size, sha256}. The card names the file, its size and \
                 whether it replaces an attachment."
            }
            VaultTool::FolderList => {
                "List the vault's folders as {folders: [{id, name}]}. The call is held."
            }
            VaultTool::FolderCreate => {
                "Create a folder. The call is held; the card names the folder."
            }
            VaultTool::FolderUpdate => {
                "Rename a folder. The call is held; the card names the folder and the new name."
            }
            VaultTool::FolderDelete => {
                "Delete a folder permanently. Its items are not deleted; they move to No \
                 Folder. The call is held; the card says so plainly."
            }
        }
    }

    fn schema(tool: VaultTool) -> Value {
        match tool {
            VaultTool::Search => serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Text to match against item name, username or URI." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum rows to return; defaults to 20." }
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
            VaultTool::Get => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The item's id." },
                    "reveal": {
                        "type": "string",
                        "pattern": "^(password|notes|card\\.(cardholder|number|expiry|code)|custom:.+)$",
                        "description": "One field to reveal: password, notes, custom:<name>, card.cardholder, card.number, card.expiry or card.code."
                    }
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
            VaultTool::Create => {
                let mut properties = serde_json::Map::new();
                properties.insert("type".to_owned(), serde_json::json!({ "type": "string", "enum": ["login", "note", "card"], "description": "Item type; defaults to login." }));
                properties.insert("name".to_owned(), serde_json::json!({ "type": "string" }));
                for (key, description) in [
                    ("username", "Login username."),
                    ("password", "Login password."),
                    ("uri", "Login URI."),
                    ("totp", "Login TOTP seed: an otpauth:// URI or the bare base32 seed."),
                    ("notes", "Notes for any type."),
                    ("cardholder_name", "Card cardholder."),
                    ("number", "Card number."),
                    ("exp_month", "Card expiry month."),
                    ("exp_year", "Card expiry year."),
                    ("code", "Card CVV."),
                    ("brand", "Card brand."),
                    ("folder_id", "Folder id."),
                ] {
                    properties.insert(
                        key.to_owned(),
                        serde_json::json!({ "type": "string", "description": description }),
                    );
                }
                properties.insert(
                    "custom_fields".to_owned(),
                    serde_json::json!({
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "value": { "type": "string" },
                                "type": { "type": "string", "enum": ["text", "hidden", "boolean"] }
                            },
                            "required": ["name", "value"],
                            "additionalProperties": false,
                        }
                    }),
                );
                serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": ["name"],
                    "additionalProperties": false,
                })
            }
            VaultTool::Update => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The item's id." },
                    "patch": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "username": { "type": "string" },
                            "password": { "type": "string" },
                            "uri": { "type": "string" },
                            "totp": { "type": "string" },
                            "notes": { "type": "string" },
                            "cardholder_name": { "type": "string" },
                            "number": { "type": "string" },
                            "exp_month": { "type": "string" },
                            "exp_year": { "type": "string" },
                            "code": { "type": "string" },
                            "brand": { "type": "string" },
                            "folder_id": { "type": "string" },
                            "custom_fields": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "value": { "type": "string" },
                                        "type": { "type": "string", "enum": ["text", "hidden", "boolean"] }
                                    },
                                    "required": ["name", "value"],
                                    "additionalProperties": false,
                                }
                            }
                        },
                        "additionalProperties": false,
                    }
                },
                "required": ["id", "patch"],
                "additionalProperties": false,
            }),
            VaultTool::Trash | VaultTool::Restore | VaultTool::TotpCode => serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "The item's id." } },
                "required": ["id"],
                "additionalProperties": false,
            }),
            VaultTool::AttachmentGet => serde_json::json!({
                "type": "object",
                "properties": {
                    "item_id": { "type": "string" },
                    "attachment_id": { "type": "string" },
                    "destination": { "type": "string", "description": "A path inside this session's working directory." }
                },
                "required": ["item_id", "attachment_id", "destination"],
                "additionalProperties": false,
            }),
            VaultTool::AttachmentPut => serde_json::json!({
                "type": "object",
                "properties": {
                    "item_id": { "type": "string" },
                    "path": { "type": "string", "description": "A file inside this session's working directory." },
                    "name": { "type": "string", "description": "The attachment name; defaults to the file's own name." },
                    "replace_attachment_id": { "type": "string", "description": "The attachment to remove after the new one uploads." }
                },
                "required": ["item_id", "path"],
                "additionalProperties": false,
            }),
            VaultTool::FolderList => serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            VaultTool::FolderCreate => serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false,
            }),
            VaultTool::FolderUpdate => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" }
                },
                "required": ["id", "name"],
                "additionalProperties": false,
            }),
            VaultTool::FolderDelete => serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false,
            }),
        }
    }

    async fn invoke(&self, tool: VaultTool, input: Value) -> ToolOutput {
        match tool {
            VaultTool::Search => {
                let args = match parse::<SearchArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.search(&self.slot, &args.query, args.limit).await)
            }
            VaultTool::Get => {
                let args = match parse::<GetArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.get(&self.slot, &args.id, args.reveal.as_deref()).await)
            }
            VaultTool::Create => {
                let args = match parse::<CreateInput>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.create(&self.slot, &args).await)
            }
            VaultTool::Update => {
                #[derive(serde::Deserialize)]
                struct Args {
                    id: String,
                    patch: PatchInput,
                }
                let args = match parse::<Args>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.update(&self.slot, &args.id, &args.patch).await)
            }
            VaultTool::Trash | VaultTool::Restore | VaultTool::TotpCode => {
                let args = match parse::<IdArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(match tool {
                    VaultTool::Trash => self.facade.trash(&self.slot, &args.id).await,
                    VaultTool::Restore => self.facade.restore(&self.slot, &args.id).await,
                    _ => self.facade.totp_code(&self.slot, &args.id).await,
                })
            }
            VaultTool::AttachmentGet => {
                let args = match parse::<AttachmentGetArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(
                    self.facade
                        .attachment_get(
                            &self.slot,
                            &args.item_id,
                            &args.attachment_id,
                            &args.destination,
                        )
                        .await,
                )
            }
            VaultTool::AttachmentPut => {
                let args = match parse::<AttachmentPutArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(
                    self.facade
                        .attachment_put(
                            &self.slot,
                            &args.item_id,
                            &args.path,
                            args.name.as_deref(),
                            args.replace_attachment_id.as_deref(),
                        )
                        .await,
                )
            }
            VaultTool::FolderList => reply(self.facade.folder_list(&self.slot).await),
            VaultTool::FolderCreate => {
                let args = match parse::<FolderNameArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.folder_create(&self.slot, &args.name).await)
            }
            VaultTool::FolderUpdate => {
                let args = match parse::<FolderUpdateArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.folder_update(&self.slot, &args.id, &args.name).await)
            }
            VaultTool::FolderDelete => {
                let args = match parse::<IdArgs>(input) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                reply(self.facade.folder_delete(&self.slot, &args.id).await)
            }
        }
    }
}

impl ToolProvider for VaultTools {
    fn tools(&self) -> Vec<Tool> {
        VaultTool::ALL
            .into_iter()
            .map(|tool| Tool {
                name: tool.as_str().to_owned(),
                description: Self::describe(tool).to_owned(),
                schema: Self::schema(tool),
            })
            .collect()
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        input: Value,
    ) -> BoxFuture<'a, Option<Result<ToolOutput, String>>> {
        Box::pin(async move {
            // A name this family does not own is not its call to answer.
            let tool = VaultTool::parse(name)?;
            Some(Ok(self.invoke(tool, input).await))
        })
    }
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(serde::Deserialize)]
struct GetArgs {
    id: String,
    #[serde(default)]
    reveal: Option<String>,
}

#[derive(serde::Deserialize)]
struct IdArgs {
    id: String,
}

#[derive(serde::Deserialize)]
struct AttachmentGetArgs {
    item_id: String,
    attachment_id: String,
    destination: String,
}

#[derive(serde::Deserialize)]
struct AttachmentPutArgs {
    item_id: String,
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    replace_attachment_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct FolderNameArgs {
    name: String,
}

#[derive(serde::Deserialize)]
struct FolderUpdateArgs {
    id: String,
    name: String,
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(value)
        .map_err(|error| ToolOutput::error(format!("invalid arguments: {error}")))
}

fn reply(result: Result<Value, VaultToolError>) -> ToolOutput {
    match result {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(json) => ToolOutput::text(json),
            Err(error) => ToolOutput::error(format!("the result did not serialise: {error}")),
        },
        Err(error) => ToolOutput::error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::MockVaultFacade;
    use serde_json::json;

    async fn call(tool: VaultTool, args: Value) -> (ToolOutput, Vec<String>) {
        let facade = Arc::new(MockVaultFacade::new());
        let family = VaultTools::new(
            Arc::clone(&facade) as Arc<dyn VaultFacade>,
            SessionSlot::lead("Default", "forge"),
        );
        let output = family
            .call(tool.as_str(), args)
            .await
            .expect("the family owns its own tool")
            .expect("the call answers");
        (output, facade.calls())
    }

    /// Every schema is closed: a field the tool does not name is refused by
    /// the CLI rather than silently ignored.
    #[test]
    fn every_tool_schema_is_closed() {
        let facade = Arc::new(MockVaultFacade::new());
        let family =
            VaultTools::new(facade as Arc<dyn VaultFacade>, SessionSlot::lead("Default", "forge"));
        let described = family.tools();
        assert_eq!(described.len(), VaultTool::ALL.len(), "one description per tool");
        for (tool, described) in VaultTool::ALL.into_iter().zip(described) {
            assert_eq!(described.name, tool.as_str());
            assert!(!described.description.is_empty());
            assert_eq!(
                described.schema["additionalProperties"],
                json!(false),
                "{} is not closed",
                tool.as_str(),
            );
        }
    }

    /// The tool forwards the query and returns the facade's answer.
    #[tokio::test]
    async fn search_forwards_the_query() {
        let (output, calls) = call(VaultTool::Search, json!({ "query": "bank" })).await;
        assert!(!output.is_error, "{output:?}");
        assert_eq!(calls, vec!["vault__search"]);
        let text = format!("{output:?}");
        assert!(text.contains("vault__search"), "{text}");
    }

    /// The Get schema admits the `custom:<name>` reveal the tool promises;
    /// an enum of the fixed names would reject it before the facade could.
    #[test]
    fn the_get_schema_admits_a_custom_reveal() {
        let schema = VaultTools::schema(VaultTool::Get);
        let pattern = schema["properties"]["reveal"]["pattern"]
            .as_str()
            .expect("the reveal carries a pattern");
        assert!(pattern.contains("custom:"), "the custom reveal is admitted: {pattern}");
        assert!(schema["properties"]["reveal"].get("enum").is_none());
    }

    /// A seed reaches the facade only if the schema names it: the schemas are
    /// closed, so an unnamed argument is refused by the CLI before the call.
    #[test]
    fn the_create_and_update_schemas_accept_a_totp_seed() {
        let create = VaultTools::schema(VaultTool::Create);
        let update = VaultTools::schema(VaultTool::Update);

        assert!(create["properties"].get("totp").is_some(), "create names totp");
        assert!(
            update["properties"]["patch"]["properties"].get("totp").is_some(),
            "update names totp",
        );
    }

    /// A malformed argument fails before the facade runs.
    #[tokio::test]
    async fn a_malformed_argument_is_a_tool_error() {
        let (output, calls) = call(VaultTool::Get, json!({ "reveal": "password" })).await;
        assert!(output.is_error);
        assert!(format!("{output:?}").contains("invalid arguments"), "{output:?}");
        assert!(calls.is_empty(), "the facade must not run for a malformed call");
    }

    /// The facade's refusal is surfaced as an error with its own words.
    #[tokio::test]
    async fn a_facade_refusal_is_a_tool_error() {
        let facade = Arc::new(MockVaultFacade::new());
        facade.fail_next("the vault is locked");
        let family = VaultTools::new(
            Arc::clone(&facade) as Arc<dyn VaultFacade>,
            SessionSlot::lead("Default", "forge"),
        );
        let output = family
            .call("vault__folder_list", json!({}))
            .await
            .expect("the family owns the tool")
            .expect("the call answers");
        assert!(output.is_error);
        assert!(format!("{output:?}").contains("the vault is locked"), "{output:?}");
    }

    /// The family registers all thirteen tools when the host asks what it
    /// offers.
    #[test]
    fn all_thirteen_tools_register() {
        let facade = Arc::new(MockVaultFacade::new());
        let family =
            VaultTools::new(facade as Arc<dyn VaultFacade>, SessionSlot::lead("Default", "forge"));
        let names: Vec<String> = family.tools().into_iter().map(|tool| tool.name).collect();
        for tool in VaultTool::ALL {
            assert!(names.contains(&tool.as_str().to_owned()), "{} missing", tool.as_str());
        }
    }

    /// A name the family does not own is not its call to answer, so a host
    /// composing several providers reaches the one that does.
    #[tokio::test]
    async fn an_unknown_tool_is_not_mine() {
        let facade = Arc::new(MockVaultFacade::new());
        let family =
            VaultTools::new(facade as Arc<dyn VaultFacade>, SessionSlot::lead("Default", "forge"));
        assert!(family.call("agents__spawn", json!({})).await.is_none());
    }
}
