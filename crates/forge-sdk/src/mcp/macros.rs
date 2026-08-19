//! Declarative `tool!` macro for convenient Tool-trait impls.
//!
//! See [`crate::tool!`] for usage.

/// Declarative tool-trait impl.
///
/// # Example
///
/// ```ignore
/// use forge_sdk::tool;
/// use forge_sdk::mcp::{ToolInput, ToolOutput};
/// use serde_json::json;
///
/// tool! {
///     name: "greet",
///     description: "Greet by name",
///     schema: json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
///     call: |input: ToolInput| async move {
///         let name = input.value["name"].as_str().unwrap_or("stranger");
///         ToolOutput::text(format!("hello {name}"))
///     },
///     tool_type: GreetTool,
/// }
/// ```
#[macro_export]
macro_rules! tool {
    (
        name: $name:literal,
        description: $desc:literal,
        schema: $schema:expr,
        call: |$input:ident : $input_ty:ty| async move $body:block,
        tool_type: $ty:ident $(,)?
    ) => {
        #[derive(Clone, Copy, Debug, Default)]
        // The caller names this struct, so it cannot carry a doc comment
        // the macro wrote.
        #[allow(missing_docs)]
        pub struct $ty;

        #[$crate::__private::async_trait]
        impl $crate::mcp::Tool for $ty {
            fn name(&self) -> &'static str {
                $name
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn input_schema(&self) -> ::serde_json::Value {
                $schema
            }
            async fn call(&self, $input: $input_ty) -> $crate::mcp::ToolOutput {
                $body
            }
        }
    };
}

pub mod __private {
    pub use async_trait::async_trait;
}
