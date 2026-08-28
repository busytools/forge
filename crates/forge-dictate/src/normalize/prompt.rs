//! The exact input format S1-mini was trained on.

/// Required verbatim. The model card states that changing this wording,
/// or dropping the control line below, can produce garbled output.
pub const SYSTEM: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

/// What the Qwen3 template emits for `enable_thinking=false`. Omit it and
/// the model answers with this fragment and stops, which reads as a broken
/// pipeline rather than a malformed prompt.
pub const ASSISTANT_PREFIX: &str = "<|im_start|>assistant\n<think>\n\n</think>\n\n";

/// The register the transcript is rewritten into.
///
/// A closed set: the card documents a value outside the trained sets as one
/// of the two ways to get garbled output, so the set is enforced here rather
/// than trusted to callers.
///
/// The four are not a gradient. Measured, `Formal` declines comma insertions
/// and a grammar fix that `SemiFormal` applies, so the card's "`formal` is
/// `semi-formal` with contractions expanded" is wrong and there is no
/// dialling this a little further in either direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Styling {
    /// Lowercase throughout, apostrophes stripped, colloquialisms kept.
    /// Measured to mangle technical terms, rendering MMAP as "mmapped here".
    Casual,
    /// Keeps the speaker's phrasing; sentence starts stay lowercase.
    SemiCasual,
    /// Standard written English, colloquialisms smoothed. The only value
    /// measured to insert commas the transcript did not have.
    #[default]
    SemiFormal,
    /// As `SemiFormal`, with contractions expanded.
    Formal,
}

impl Styling {
    /// The card's spelling. These strings are the trained vocabulary, not a
    /// display format.
    fn as_str(self) -> &'static str {
        match self {
            Self::Casual => "casual",
            Self::SemiCasual => "semi-casual",
            Self::SemiFormal => "semi-formal",
            Self::Formal => "formal",
        }
    }
}

/// Whether the model may turn enumerable content into a bulleted list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Structure {
    /// Sentences and paragraphs throughout.
    #[default]
    Prose,
    /// Permits Markdown bullets. **Returns multi-line text** when it forms
    /// a list, so a host that assumes one line gets a surprise. Measured
    /// more conservative than the card documents: the card's own three-item
    /// example comes back as prose here, while a seven-item input bullets.
    Lists,
}

impl Structure {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prose => "prose",
            Self::Lists => "lists",
        }
    }
}

/// Destination conventions. Not a domain or vocabulary hint: it will not
/// protect technical terms, which are a [`Styling`] concern.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Context {
    /// Flowing text.
    #[default]
    General,
    /// Email layout: a greeting line, the body and a sign-off block,
    /// separated by blank lines. **Reliably returns multi-line text**, so a
    /// host that assumes one line gets a surprise. Measured token-neutral
    /// rather than longer, because the newlines replace connective prose.
    Email,
}

impl Context {
    fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Email => "email",
        }
    }
}

pub fn build(text: &str, styling: Styling, structure: Structure, context: Context) -> String {
    let styling = styling.as_str();
    let structure = structure.as_str();
    let context = context.as_str();
    format!(
        "<|im_start|>system\n{SYSTEM}<|im_end|>\n\
         <|im_start|>user\n[Styling: {styling}] [Structure: {structure}] [Context: {context}]\n\
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
            build(
                "so um send the report by uh friday",
                Styling::SemiFormal,
                Structure::Prose,
                Context::General,
            ),
            CARD_EXAMPLE,
            "prompt diverged from the format S1-mini was trained on"
        );
    }

    /// The control line has to carry the caller's axes in the card's own
    /// spelling. A stringly-typed axis would let a plausible-looking
    /// `"semiformal"` through to the model, which the card lists as a cause
    /// of garbled output.
    #[test]
    fn the_axes_reach_the_control_line_in_the_cards_spelling() {
        for (styling, spelling) in [
            (Styling::Casual, "casual"),
            (Styling::SemiCasual, "semi-casual"),
            (Styling::SemiFormal, "semi-formal"),
            (Styling::Formal, "formal"),
        ] {
            let p = build("anything", styling, Structure::Prose, Context::General);
            assert!(
                p.contains(&format!("[Styling: {spelling}]")),
                "{styling:?} did not reach the control line as {spelling:?}"
            );
        }
        for (structure, spelling) in [(Structure::Prose, "prose"), (Structure::Lists, "lists")] {
            let p = build("anything", Styling::SemiFormal, structure, Context::General);
            assert!(
                p.contains(&format!("[Structure: {spelling}]")),
                "{structure:?} did not reach the control line as {spelling:?}"
            );
        }
        for (context, spelling) in [(Context::General, "general"), (Context::Email, "email")] {
            let p = build("anything", Styling::SemiFormal, Structure::Prose, context);
            assert!(
                p.contains(&format!("[Context: {spelling}]")),
                "{context:?} did not reach the control line as {spelling:?}"
            );
        }
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
            build("anything", Styling::default(), Structure::default(), Context::default())
                .ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"),
            "prompt lost the empty think block; the model will answer \
             `<think>` and stop"
        );
    }
}
