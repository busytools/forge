//! Prompt-lookup speculative decoding.
//!
//! A normalized transcript is nearly its input, so the input is already a
//! good draft of the output. Guess the next `K` tokens by finding where the
//! text just emitted last appeared in the input, validate the whole guess
//! in one batched decode, keep the longest prefix the model agrees with,
//! and drop the KV entries behind the rest.
//!
//! Reading tokens is near-free next to generating them, so checking `K`
//! guesses costs about what generating one does and every correct guess is
//! free. Acceptance FALLS as `K` rises while wall-clock keeps improving,
//! because the longer accepted runs outweigh the wasted drafts: tune this
//! on wall-clock, never on the acceptance rate.

use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use super::NormalizeError;

/// Match width. Two beats one decisively; three gains nothing.
pub const NGRAM: usize = 2;

/// Draft length, chosen on wall-clock.
pub const K: usize = 64;

/// The tokens following the most recent occurrence of `generated`'s last
/// `ngram` tokens within `source`, capped at `k`.
///
/// Most recent rather than first: the nearest match is the better predictor
/// when a phrase repeats.
pub fn draft<'a>(
    source: &'a [LlamaToken],
    generated: &[LlamaToken],
    ngram: usize,
    k: usize,
) -> &'a [LlamaToken] {
    if ngram == 0 || generated.len() < ngram || source.len() <= ngram {
        return &[];
    }
    let tail = &generated[generated.len() - ngram..];
    for start in (0..=source.len() - ngram).rev() {
        if &source[start..start + ngram] == tail {
            let from = start + ngram;
            let to = (from + k).min(source.len());
            if from < to {
                return &source[from..to];
            }
        }
    }
    &[]
}

/// Greedy generation, speculating from `source`.
///
/// `ctx` must already hold the decoded prompt, and `start` is the position
/// the next token occupies.
pub fn generate(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    batch: &mut LlamaBatch,
    source: &[LlamaToken],
    start: i32,
    budget: usize,
) -> Result<String, NormalizeError> {
    let mut sampler = LlamaSampler::greedy();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut out = String::new();
    let mut emitted: Vec<LlamaToken> = Vec::new();
    let mut pos = start;
    let mut current = sampler.sample(ctx, batch.n_tokens() - 1);

    while emitted.len() < budget {
        if model.is_eog_token(current) {
            break;
        }
        sampler.accept(current);
        out.push_str(&model.token_to_piece(current, &mut decoder, false, None)?);
        emitted.push(current);

        let guess = draft(source, &emitted, NGRAM, K);

        // The confirmed token plus the entire draft in one decode. Every
        // position asks for logits, so one pass validates every guess.
        batch.clear();
        batch.add(current, pos, &[0], true)?;
        for (i, token) in guess.iter().enumerate() {
            let offset = i32::try_from(i).unwrap_or(i32::MAX);
            batch.add(*token, pos + 1 + offset, &[0], true)?;
        }
        ctx.decode(batch)?;

        // Sampling at index i gives the token following position pos + i.
        let mut taken = 0usize;
        let mut next = sampler.sample(ctx, 0);
        // Tokenizing parses special tokens, so a transcript containing a
        // literal end-of-turn marker puts a real EOG token in the draft.
        // Accepting one would detokenize a control token, which asks for no
        // bytes and fails the call with `UnknownTokenType`.
        while taken < guess.len()
            && next == guess[taken]
            && !model.is_eog_token(next)
            && emitted.len() < budget
        {
            sampler.accept(next);
            out.push_str(&model.token_to_piece(next, &mut decoder, false, None)?);
            emitted.push(next);
            taken += 1;
            next = sampler.sample(ctx, i32::try_from(taken).unwrap_or(i32::MAX));
        }

        // Positions from pos+1+taken onwards hold drafts the model refused.
        // Leaving them attends over tokens that were never emitted, which
        // is the one error that makes this diverge from plain greedy.
        if taken < guess.len() {
            let first_stale = u32::try_from(pos + 1)
                .unwrap_or(u32::MAX)
                .saturating_add(u32::try_from(taken).unwrap_or(u32::MAX));
            ctx.kv_cache_seq_rm(0, Some(first_stale), None)?;
        }

        pos += 1 + i32::try_from(taken).unwrap_or(i32::MAX);
        current = next;
    }

    // Stopping here rather than on an end-of-generation token means the
    // text is cut off mid-sentence. Nothing downstream can tell, and the
    // byte-identical gate cannot either, because both paths share `budget`.
    // Measured clear under `Structure: prose`; `lists` is the setting that
    // expands output into bullets, so it is where this would first fire.
    if emitted.len() >= budget {
        tracing::warn!(budget, "normalization hit its token ceiling; output is truncated");
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(ids: &[i32]) -> Vec<LlamaToken> {
        ids.iter().copied().map(LlamaToken).collect()
    }

    /// A forward search would return the continuation of the FIRST match,
    /// which is the wrong one and the cheaper thing to write by accident.
    #[test]
    fn drafts_from_the_most_recent_match() {
        let source = tokens(&[1, 2, 99, 1, 2, 42, 43]);
        let generated = tokens(&[7, 1, 2]);
        assert_eq!(
            draft(&source, &generated, 2, 4),
            tokens(&[42, 43]).as_slice(),
            "draft did not continue from the last occurrence of the tail"
        );
    }

    #[test]
    fn draft_is_capped_at_k() {
        let source = tokens(&[1, 2, 3, 4, 5, 6]);
        let generated = tokens(&[1, 2]);
        assert_eq!(
            draft(&source, &generated, 2, 2),
            tokens(&[3, 4]).as_slice(),
            "draft exceeded k tokens"
        );
    }

    /// Nothing to speculate from is the common case on the first token, and
    /// it has to be empty rather than a panic on an underflowed range.
    #[test]
    fn no_draft_without_a_match() {
        let source = tokens(&[1, 2, 3]);
        assert!(
            draft(&source, &tokens(&[8, 9]), 2, 4).is_empty(),
            "drafted from a tail that does not occur in the source"
        );
        assert!(
            draft(&source, &tokens(&[1]), 2, 4).is_empty(),
            "drafted from fewer generated tokens than the ngram width"
        );
    }
}
