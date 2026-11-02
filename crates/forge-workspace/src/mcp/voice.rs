//! Voice MCP - speak one utterance to the person listening to this seat
//! (`mcp__forge__voice__say`).
//!
//! The seventh toggleable family, so a lead can withhold it from a worker
//! that should not speak. This tool is the only way the agent's words become
//! speech: the chat it writes is never spoken.

use std::sync::Arc;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use crate::SessionSlot;
use crate::voice::{SayError, VoiceService};

/// Attach `voice__say` to an existing [`McpServerBuilder`].
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    voice: Arc<VoiceService>,
    slot: SessionSlot,
) -> McpServerBuilder {
    builder.tool(Say { voice, slot })
}

struct Say {
    voice: Arc<VoiceService>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    text: String,
}

#[async_trait::async_trait]
impl Tool for Say {
    fn name(&self) -> &'static str {
        "voice__say"
    }

    fn description(&self) -> &'static str {
        "Speak one utterance to the person listening to this seat. Use it for anything they should \
         hear; the text you write in the chat is not spoken. Returns `spoken: queued` when it is \
         queued, or `no listener` when no client has voice on for this seat."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "One utterance, typically a sentence or a short paragraph.",
                },
            },
            "required": ["text"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let Args { text } = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(error) => return ToolOutput::error(format!("invalid arguments: {error}")),
        };
        if text.trim().is_empty() {
            return ToolOutput::error("nothing to say: the utterance is empty");
        }
        match self.voice.say(&self.slot, text).await {
            Ok(()) => ToolOutput::text("spoken: queued"),
            Err(SayError::NoListener) => ToolOutput::text("no listener"),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::SessionSlot;
    use forge_sdk::mcp::server::McpServerBuilder;
    use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
    use forge_tts::Chunk;

    use super::{Say, add_tools};
    use crate::voice::{AudioSink, NoteDispatch, VoiceService, VoiceSettings};

    fn slot() -> SessionSlot {
        SessionSlot::new("Org", "project", "lead")
    }

    struct SilentSink;
    impl AudioSink for SilentSink {
        fn push(&self, _seat: &SessionSlot, _chunk: Chunk) {}
    }

    struct SilentNotes;
    impl NoteDispatch for SilentNotes {
        fn note(&self, _seat: &SessionSlot, _text: &str) {}
    }

    fn service() -> Arc<VoiceService> {
        let loader = Arc::new(|| Err("no model loads in this test".to_owned()));
        Arc::new(VoiceService::new(
            loader,
            VoiceSettings::default(),
            Arc::new(SilentSink) as Arc<dyn AudioSink>,
            Arc::new(SilentNotes) as Arc<dyn NoteDispatch>,
        ))
    }

    async fn say(text: &str) -> ToolOutput {
        Say { voice: service(), slot: slot() }
            .call(ToolInput { value: serde_json::json!({ "text": text }) })
            .await
    }

    #[tokio::test]
    async fn a_say_with_no_listener_answers_no_listener() {
        let output = say("Hello.").await;
        assert!(!output.is_error, "no listener is an answer, not a failure: {output:?}");
        let ToolOutputBlock::Text { text } = &output.blocks[0] else {
            panic!("the answer is a line of text");
        };
        assert_eq!(text, "no listener");
    }

    #[tokio::test]
    async fn an_empty_text_is_refused() {
        let output = say("   ").await;
        assert!(output.is_error, "an empty utterance is refused");
    }

    #[test]
    fn the_family_registers_one_tool_named_voice_say() {
        let server = add_tools(McpServerBuilder::new("forge", "0.0.0"), service(), slot()).build();
        assert_eq!(server.tool_names(), vec!["voice__say".to_owned()]);
    }
}
