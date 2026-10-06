//! The driver: upstream `@playwright/mcp`, run as a child process and spoken
//! to as an MCP CLIENT.
//!
//! **Upstream before building** (rule 23): the actionability, the snapshot
//! with refs and the page-state the tools depend on are Playwright's, so the
//! driver IS Playwright's own MCP server, pointed at the browser this host
//! launched. `rmcp` carries the child process, the id correlation and the
//! cancellation, so this module is the wiring and the mapping and nothing
//! more.
//!
//! **`--no-webmcp` is what keeps the surface deterministic**: without it the
//! driver registers whatever tools a page offers through WebMCP and emits
//! `tools/list_changed`, so what a session sees would depend on what the page
//! it happens to have open chooses to expose.

use std::path::{Path, PathBuf};

use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt as _, TokioChildProcess};
use serde_json::Value;

/// One part of a tool's answer, on its way to the socket.
///
/// An image's bytes cross here as the base64 the MCP client's own model
/// carries: the socket wants them as a binary frame, and the frontend decodes
/// them into one.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplyPart {
    Text { text: String },
    Image { mime_type: String, data_base64: String },
}

/// The driver child, and the MCP client speaking to it.
pub struct Driver {
    /// Kept so the child is killed when the driver goes: a relaunch must not
    /// leave the old one holding the browser.
    _service: RunningService<rmcp::RoleClient, ()>,
    client: rmcp::Peer<rmcp::RoleClient>,
}

impl Driver {
    /// Start the driver against a browser's CDP endpoint.
    ///
    /// `context` picks which context this driver drives. `Some(path)` is a
    /// NAMED context: the driver creates one of its own, isolated from every
    /// other driver on the browser, and reads its cookies from `path` -
    /// created empty when it is not there, since upstream reads the file at
    /// context creation and a missing one fails the launch. `None` attaches
    /// to the browser's own context, the profile's, which outlives everything.
    pub async fn start(
        node: &Path,
        cli: &Path,
        cdp_endpoint: &str,
        output_dir: &Path,
        context: Option<&Path>,
    ) -> Result<Self, String> {
        if !node.is_file() || !cli.is_file() {
            return Err(format!(
                "the vendored driver is not there ({} / {}) - run `just vendor-browser-stack`",
                node.display(),
                cli.display(),
            ));
        }
        std::fs::create_dir_all(output_dir)
            .map_err(|why| format!("the browser output directory cannot be made: {why}"))?;
        if let Some(storage) = context {
            ensure_storage_state(storage)?;
        }

        let transport =
            TokioChildProcess::new(tokio::process::Command::new(node).configure(|cmd| {
                cmd.arg(cli);
                if let Some(storage) = context {
                    cmd.arg("--isolated").arg("--storage-state").arg(storage);
                }
                cmd.arg("--cdp-endpoint")
                    .arg(cdp_endpoint)
                    .arg("--no-webmcp")
                    // Where upstream's own screenshot default lands: the
                    // client runs from a directory nobody chose, and a file
                    // dropped there is one nobody finds.
                    .arg("--output-dir")
                    .arg(output_dir);
            }))
            .map_err(|why| format!("the driver would not start: {why}"))?;

        let service = ()
            .serve(transport)
            .await
            .map_err(|why| format!("the driver did not answer its MCP handshake: {why}"))?;
        let client = service.peer().clone();
        Ok(Self { _service: service, client })
    }

    /// Whether the child is still there to answer.
    pub fn is_running(&self) -> bool {
        !self.client.is_transport_closed()
    }

    /// Run one tool, answering with its parts or the reason it failed.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Vec<ReplyPart>, String> {
        let arguments = match args {
            Value::Object(fields) => fields,
            Value::Null => serde_json::Map::new(),
            other => {
                return Err(format!(
                    "{tool} was called with arguments that are not an object: {other}"
                ));
            }
        };
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
            .await
            .map_err(|why| format!("the driver did not answer {tool}: {why}"))?;

        let parts = parts_of(&result)?;
        if result.is_error == Some(true) {
            // Upstream reports a failed call as an error result whose content
            // is the reason; the socket's failure arm carries the words.
            let reason = parts
                .iter()
                .map(|part| match part {
                    ReplyPart::Text { text } => text.clone(),
                    ReplyPart::Image { .. } => "[an image]".to_owned(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if reason.is_empty() { format!("{tool} failed") } else { reason });
        }
        Ok(parts)
    }
}

/// Map one MCP result onto the parts the socket carries.
///
/// **A block this host cannot carry is an error rather than a silence.** The
/// tools phase 1 registers answer with text and images; a resource or an
/// audio block is something no view could draw and no rule allows dropping,
/// so the call fails naming it.
fn parts_of(result: &rmcp::model::CallToolResult) -> Result<Vec<ReplyPart>, String> {
    use rmcp::model::ContentBlock;

    let mut parts = Vec::with_capacity(result.content.len());
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(ReplyPart::Text { text: text.text.clone() }),
            ContentBlock::Image(image) => parts.push(ReplyPart::Image {
                mime_type: image.mime_type.clone(),
                data_base64: image.data.clone(),
            }),
            other => {
                return Err(format!(
                    "the driver answered with a block this host cannot carry: {other:?}"
                ));
            }
        }
    }
    Ok(parts)
}

/// Write the empty storage state a named context starts from, if it is not
/// there.
fn ensure_storage_state(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|why| format!("the context's storage directory cannot be made: {why}"))?;
    }
    std::fs::write(path, r#"{"cookies": [], "origins": []}"#)
        .map_err(|why| format!("the context's storage state cannot be written: {why}"))
}

/// The CLI inside the vendored package, run by the vendored node.
pub fn cli_path(stack: &Path) -> PathBuf {
    stack.join("playwright-mcp/node_modules/@playwright/mcp/cli.js")
}

/// The vendored node.
pub fn node_path(stack: &Path) -> PathBuf {
    stack.join("node/bin/node")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolResult;

    /// A text answer is its text, and a result flagged as an error is not a
    /// result at all: it is the call's reason, which the tool returns as a
    /// failure.
    #[test]
    fn a_text_answer_maps_to_text_and_an_error_result_carries_its_reason() {
        use rmcp::model::ContentBlock;

        let result = CallToolResult::success(vec![ContentBlock::text("navigated")]);
        assert_eq!(parts_of(&result), Ok(vec![ReplyPart::Text { text: "navigated".to_owned() }]));

        let failed = CallToolResult::error(vec![ContentBlock::text("no such element")]);
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            parts_of(&failed),
            Ok(vec![ReplyPart::Text { text: "no such element".to_owned() }]),
        );
    }

    /// An image crosses with its mime and its bytes base64 - the shape the
    /// frontend decodes into the binary frame the socket carries.
    #[test]
    fn an_image_answer_maps_to_an_image_part() {
        use rmcp::model::ContentBlock;

        let result = CallToolResult::success(vec![ContentBlock::image("AP8Q", "image/png")]);
        assert_eq!(
            parts_of(&result),
            Ok(vec![ReplyPart::Image {
                mime_type: "image/png".to_owned(),
                data_base64: "AP8Q".to_owned(),
            }]),
        );
    }

    /// A block nothing here can carry is an error naming it, not a part
    /// quietly left out: a dropped block is a result a person cannot see was
    /// ever there.
    #[test]
    fn a_block_this_host_cannot_carry_fails_the_call() {
        let result = CallToolResult::success(vec![rmcp::model::ContentBlock::Audio(
            rmcp::model::AudioContent::new("AP8Q", "audio/wav"),
        )]);
        let refused = parts_of(&result).expect_err("an audio block is not carried");
        assert!(refused.contains("cannot carry"), "{refused}");
    }

    /// A named context's storage file is created empty when it is missing and
    /// left alone when it exists: upstream reads it at context creation (a
    /// missing file fails the launch), and it is where that context's own
    /// logins live, so an existing one is never overwritten.
    #[test]
    fn a_missing_storage_state_is_created_empty_and_an_existing_one_is_kept() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = dir.path().join("contexts/alpha.json");

        ensure_storage_state(&state).expect("the missing file is created");
        let written = std::fs::read_to_string(&state).expect("the storage state is there");
        let parsed: serde_json::Value =
            serde_json::from_str(&written).expect("the written state is JSON");
        assert_eq!(parsed["cookies"], serde_json::json!([]), "{written}");
        assert_eq!(parsed["origins"], serde_json::json!([]), "{written}");

        std::fs::write(&state, r#"{"cookies": [{"name": "who"}]}"#).expect("a state with a login");
        ensure_storage_state(&state).expect("an existing file is not an error");
        assert!(
            std::fs::read_to_string(&state).expect("read back").contains("who"),
            "the context's own logins are never overwritten",
        );
    }

    /// The driver's own paths are the vendoring's layout, pinned by name.
    #[test]
    fn the_driver_is_the_cli_the_vendoring_unpacks() {
        assert_eq!(node_path(Path::new("/stack")), Path::new("/stack/node/bin/node"));
        assert_eq!(
            cli_path(Path::new("/stack")),
            Path::new("/stack/playwright-mcp/node_modules/@playwright/mcp/cli.js"),
        );
    }
}
