//! Splitting an utterance into render units, and the bound one must fit.

use crate::Error;

/// A rough speaking pace, in characters per second, for a duration estimate.
const CHARS_PER_SECOND: u64 = 15;

/// The sentences of `text`, in order, with their punctuation.
///
/// A unit ends at a `.`, `!` or `?` followed by whitespace or the end, except
/// a period between two digits, which is a decimal. Abbreviations are not
/// recognised: a false split costs one extra render, a missed split only
/// latency.
pub fn sentences(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut start = 0;
    let mut previous_digit = false;
    for (index, character) in text.char_indices() {
        let end = index + character.len_utf8();
        let next = text[end..].chars().next();
        let ends = match character {
            '!' | '?' => next.is_none_or(char::is_whitespace),
            '.' => {
                let decimal = previous_digit && next.is_some_and(|next| next.is_ascii_digit());
                !decimal && next.is_none_or(char::is_whitespace)
            }
            _ => false,
        };
        if ends {
            push(&mut units, &text[start..end]);
            start = end;
        }
        previous_digit = character.is_ascii_digit();
    }
    push(&mut units, &text[start..]);
    units
}

/// Whether one whole utterance fits `max_seconds` at [`CHARS_PER_SECOND`].
pub fn within_bound(text: &str, max_seconds: u64) -> Result<(), Error> {
    let bound = max_seconds.saturating_mul(CHARS_PER_SECOND);
    let characters = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    if characters > bound {
        return Err(Error::Render(format!(
            "the utterance is longer than [tts] max_utterance_seconds ({max_seconds})"
        )));
    }
    Ok(())
}

fn push(units: &mut Vec<String>, unit: &str) {
    let trimmed = unit.trim();
    if !trimmed.is_empty() {
        units.push(trimmed.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{sentences, within_bound};

    #[test]
    fn three_sentences_split_into_three() {
        assert_eq!(
            sentences("One. Two! Three?"),
            vec!["One.".to_owned(), "Two!".to_owned(), "Three?".to_owned()]
        );
    }

    #[test]
    fn a_decimal_is_not_a_sentence_end() {
        assert_eq!(sentences("Version 1.2 shipped."), vec!["Version 1.2 shipped.".to_owned()]);
    }

    #[test]
    fn text_with_no_terminal_punctuation_is_one_unit() {
        assert_eq!(sentences("no punctuation here"), vec!["no punctuation here".to_owned()]);
    }

    #[test]
    fn an_over_long_utterance_is_refused_by_name() {
        let long = "word ".repeat(2_000);
        let error = within_bound(&long, 120).expect_err("refused");
        assert!(error.to_string().contains("max_utterance_seconds"));
    }
}
