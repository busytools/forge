//! `forge-mcp` - the tool-provider seam.
//!
//! A crate that owns a family of MCP tools describes them here and
//! implements [`ToolProvider`]: the name, the description and the input
//! schema a host advertises, and the call that answers one invocation. The
//! in-process MCP host stays in `forge-sdk` and consumes any provider, so
//! an owning crate never depends on the CLI subprocess crate to bring its
//! own tools.
//!
//! [`ToolOutput`] is the same type the host speaks - it is moved here, and
//! re-exported by the host at its old path, so an owned family and the SDK
//! produce one shape rather than two that a conversion has to keep in step.
//!
//! Nothing here is a host: no registry keyed by name, no JSON-RPC, no
//! transport. [`Providers`] is the one composition helper - a host holds
//! the providers it registered and asks them in order, so "which family
//! answers this name" is the provider's own answer and never a table the
//! host keeps.

use base64::Engine as _;
use futures::future::BoxFuture;
use serde_json::Value;

/// The future a [`ToolProvider::call`] returns, re-exported so an owning
/// crate names it here rather than taking its own `futures` dependency.
pub use futures::future::BoxFuture as ProviderFuture;

/// One tool as its owning family describes it: the name the model calls,
/// the description it reads, and the JSON Schema of the arguments.
///
/// Named `ToolSpec` rather than `Tool` because the host's own trait is
/// `forge_sdk::mcp::tool::Tool`; two things called `Tool` in one import
/// list is a wrong import waiting to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Output a tool returns.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Content blocks - matches MCP `tools/call` response shape.
    pub blocks: Vec<ToolOutputBlock>,
    /// Whether this represents a tool failure.
    pub is_error: bool,
}

impl ToolOutput {
    /// Build a text-only successful output.
    pub fn text(s: impl Into<String>) -> Self {
        Self { blocks: vec![ToolOutputBlock::Text { text: s.into() }], is_error: false }
    }

    /// Build a text-only failed output: the reason the call did not do what
    /// it was asked.
    pub fn error(s: impl Into<String>) -> Self {
        Self { blocks: vec![ToolOutputBlock::Text { text: s.into() }], is_error: true }
    }

    /// The JSON shape MCP expects a content list in.
    pub fn to_mcp_content(&self) -> Vec<Value> {
        self.blocks
            .iter()
            .map(|b| match b {
                ToolOutputBlock::Text { text } => {
                    serde_json::json!({ "type": "text", "text": text })
                }
                // The MCP image shape, which carries the bytes base64 - the
                // encoding the CLI's own image blocks arrive in.
                ToolOutputBlock::Image { mime_type, data } => serde_json::json!({
                    "type": "image",
                    "data": base64::engine::general_purpose::STANDARD.encode(data),
                    "mimeType": mime_type,
                }),
            })
            .collect()
    }
}

/// A single content block returned from a tool. MCP supports resources
/// too; add a variant here when the binary actually asks for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutputBlock {
    /// The text payload.
    Text { text: String },
    /// An image's bytes and the mime type they are in. Encoded as the MCP
    /// image content shape at the boundary, which carries `data` base64.
    Image { mime_type: String, data: Vec<u8> },
}

/// The envelope a family answers a delete with: the status word a reader
/// matches and the record that was removed. One shape across families, so a
/// reader that learned it on one tool reads the next the same way.
pub fn removed_record(removed: &Value) -> Value {
    serde_json::json!({ "status": "deleted", "removed": removed })
}

/// One crate's family of MCP tools.
///
/// [`ToolProvider::call`] answers `None` for a name this family does not
/// own, which is how a host composes several providers into one server: it
/// asks each in registration order and the first that claims the name
/// answers. A provider must name every tool it answers in
/// [`ToolProvider::tools`], because that list is what the host advertises.
pub trait ToolProvider: Send + Sync {
    /// Every tool this family offers, in the order it should be advertised.
    fn tools(&self) -> Vec<ToolSpec>;

    /// Answer one call. `None` means "not mine"; the host answers unknown
    /// only when no provider claims it.
    fn call<'a>(
        &'a self,
        name: &'a str,
        input: Value,
    ) -> BoxFuture<'a, Option<Result<ToolOutput, String>>>;
}

/// The providers one host serves, in registration order.
///
/// The host advertises the union of their tools and dispatches a call to
/// the first provider that claims its name - the composition is the
/// provider's own answer, so a new family is one more registration rather
/// than a row in the host's table.
#[derive(Default)]
pub struct Providers {
    providers: Vec<std::sync::Arc<dyn ToolProvider>>,
}

impl Providers {
    pub fn new(providers: Vec<std::sync::Arc<dyn ToolProvider>>) -> Self {
        Self { providers }
    }

    /// Every tool the registered families describe, in registration order.
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.providers.iter().flat_map(|provider| provider.tools()).collect()
    }

    /// Ask each provider in turn; the first that claims `name` answers.
    pub async fn call(&self, name: &str, input: Value) -> Option<Result<ToolOutput, String>> {
        for provider in &self.providers {
            if let Some(answer) = provider.call(name, input.clone()).await {
                return Some(answer);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image reaches the CLI as the MCP image content shape, which carries
    /// the bytes base64 under `data` and the mime beside them - the same shape
    /// a screenshot from any MCP server arrives in.
    #[test]
    fn an_image_block_crosses_as_an_mcp_image_content_block() {
        let output = ToolOutput {
            blocks: vec![ToolOutputBlock::Image {
                mime_type: "image/png".to_owned(),
                data: vec![0x00, 0xff, 0x10],
            }],
            is_error: false,
        };
        let content = output.to_mcp_content();
        assert_eq!(content.len(), 1, "one block in, one block out");
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/png");
        assert_eq!(content[0]["data"], "AP8Q", "the bytes base64-encoded, in order");
        assert!(!output.is_error, "an image is a result, not a failure");
    }

    /// Text and failure keep the shapes they had: a text block, and the same
    /// block marked as the call's error.
    #[test]
    fn text_and_failure_cross_as_text_blocks() {
        let output = ToolOutput::error("no browser-capable client connected");
        assert!(output.is_error, "the failure arm marks the call failed");
        let content = output.to_mcp_content();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "no browser-capable client connected");
    }

    struct FixedProvider {
        name: &'static str,
        answer: &'static str,
    }

    impl ToolProvider for FixedProvider {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: self.name.to_owned(),
                description: format!("the {} tool", self.name),
                schema: serde_json::json!({ "type": "object", "additionalProperties": false }),
            }]
        }

        fn call<'a>(
            &'a self,
            name: &'a str,
            _input: Value,
        ) -> BoxFuture<'a, Option<Result<ToolOutput, String>>> {
            Box::pin(async move { (name == self.name).then(|| Ok(ToolOutput::text(self.answer))) })
        }
    }

    /// The composition advertises every registered family's tools and asks
    /// them in order: a name the first does not own reaches the second, and
    /// a name nobody owns answers `None`.
    #[tokio::test]
    async fn the_composition_asks_each_provider_and_only_claims_its_own() {
        let providers = Providers::new(vec![
            std::sync::Arc::new(FixedProvider { name: "a__one", answer: "first" }),
            std::sync::Arc::new(FixedProvider { name: "b__two", answer: "second" }),
        ]);

        let names: Vec<String> = providers.tools().into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["a__one", "b__two"], "both families are advertised, in order");

        let second = providers.call("b__two", serde_json::json!({})).await;
        let Some(Ok(output)) = second else { panic!("the second provider answers its own name") };
        assert_eq!(
            output.blocks,
            vec![ToolOutputBlock::Text { text: "second".to_owned() }],
            "the second provider's answer crosses",
        );

        assert!(
            providers.call("nobody__owns_this", serde_json::json!({})).await.is_none(),
            "a name no provider claims is the host's unknown, not a provider's error",
        );
    }
}
