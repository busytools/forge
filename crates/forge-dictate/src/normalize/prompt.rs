//! The exact input format S1-mini was trained on.

/// Required verbatim. The model card states that changing this wording,
/// or dropping the control line below, can produce garbled output.
pub const SYSTEM: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

/// What the Qwen3 template emits for `enable_thinking=false`. Omit it and
/// the model answers with this fragment and stops, which reads as a broken
/// pipeline rather than a malformed prompt.
pub const ASSISTANT_PREFIX: &str = "<|im_start|>assistant\n<think>\n\n</think>\n\n";

/// The three control-line axes, at the card's documented defaults. Every
/// combination was trained, but a value outside the trained sets is one of
/// the two documented ways to get garbled output.
const STYLING: &str = "semi-formal";
const STRUCTURE: &str = "prose";
const CONTEXT: &str = "general";

pub fn build(text: &str) -> String {
    format!(
        "<|im_start|>system\n{SYSTEM}<|im_end|>\n\
         <|im_start|>user\n[Styling: {STYLING}] [Structure: {STRUCTURE}] [Context: {CONTEXT}]\n\
         {text}<|im_end|>\n\
         {ASSISTANT_PREFIX}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transcribed from the model card's hand-built-prompt section, not
    /// from `build`, so the two agree only if `build` is right.
    const CARD_EXAMPLE: &str = "<|im_start|>system\nYou are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.<|im_end|>\n<|im_start|>user\n[Styling: semi-formal] [Structure: prose] [Context: general]\nso um send the report by uh friday<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

    #[test]
    fn matches_the_trained_input_format() {
        assert_eq!(
            build("so um send the report by uh friday"),
            CARD_EXAMPLE,
            "prompt diverged from the format S1-mini was trained on"
        );
    }

    /// Insurance, not coverage: `CARD_EXAMPLE` already carries this prefix
    /// verbatim, so no mutation kills this without also killing the test
    /// above. It is kept for the failure message, which names the one
    /// omission that makes the model answer `<think>` and stop.
    ///
    /// Spelled out rather than compared against [`ASSISTANT_PREFIX`]: the
    /// constant is what builds the prompt, so checking one against the
    /// other passes for any value they happen to share.
    #[test]
    fn carries_the_empty_think_block() {
        assert!(
            build("anything").ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"),
            "prompt lost the empty think block; the model will answer \
             `<think>` and stop"
        );
    }
}
