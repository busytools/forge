//! Repairs raw speech-recognition output into written text.
//!
//! Punctuation, capitalization, filler removal, spoken numbers and dates
//! rendered in written form, and self-corrections resolved to whatever the
//! speaker landed on. Text in, text out: this stage never sees audio.
//!
//! The model is "S1-mini" by "Superwhisper", 596M parameters, Apache 2.0
//! plus a clause requiring it to keep that name and capitalization wherever
//! it is used. llama.cpp reports 751632384 because the tied embedding is
//! stored materialized and counted twice.

mod lookup;
mod prompt;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::{LogOptions, send_logs_to_tracing};

/// Everything the normalizer can fail with.
#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    /// llama.cpp's global backend refused to start. It is a process
    /// singleton, so the usual cause is another library in the same
    /// process having initialized it first.
    #[error("llama backend could not start: {0}")]
    Backend(String),

    /// The weights could not be opened. [`crate::prepare`] checks size and
    /// hash before anything reaches here, so a failure at this point is
    /// usually a path that names no file.
    #[error("could not load {}: {source}", path.display())]
    Load {
        path: PathBuf,
        #[source]
        source: llama_cpp_2::LlamaModelLoadError,
    },

    /// A context could not be created for the loaded weights.
    #[error("could not create an inference context: {0}")]
    Context(#[from] llama_cpp_2::LlamaContextLoadError),

    /// The input could not be turned into tokens. Interior nul bytes are
    /// the only realistic cause.
    #[error("could not tokenize the input: {0}")]
    Tokenize(#[from] llama_cpp_2::StringToTokenError),

    /// A token came back that is not valid UTF-8 on its own.
    #[error("could not decode a generated token: {0}")]
    Detokenize(#[from] llama_cpp_2::TokenToStringError),

    /// The batch would not hold the tokens offered to it.
    #[error("could not fill the decode batch: {0}")]
    Batch(#[from] llama_cpp_2::llama_batch::BatchAddError),

    /// llama.cpp rejected a decode step.
    #[error("decode failed: {0}")]
    Decode(#[from] llama_cpp_2::DecodeError),

    /// Rejected draft tokens could not be dropped from the KV cache.
    /// Continuing would attend over tokens that were never emitted.
    #[error("could not roll back the kv cache: {0}")]
    KvCache(#[from] llama_cpp_2::context::kv_cache::KvCacheConversionError),
}

/// llama.cpp's backend is global to the process and may be initialized
/// exactly once.
///
/// Routing logs is inside this initializer because it has to happen before
/// the backend starts: `send_logs_to_tracing` binds ggml's log sink as well
/// as llama's, and once the backend is up, ggml's Metal device-init block
/// has already gone to stderr. Running the identical call afterwards still
/// leaks those 16 lines, which is enough to corrupt a full-screen terminal.
/// `LlamaBackend::void_logs` never suppresses them at all: it binds only
/// llama's sink, not ggml's.
fn backend() -> Result<&'static LlamaBackend, NormalizeError> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();

    BACKEND
        .get_or_init(|| {
            send_logs_to_tracing(LogOptions::default());
            LlamaBackend::init().map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| NormalizeError::Backend(e.clone()))
}

/// Offload everything; llama.cpp clamps this to the layers that exist and
/// silently does nothing without an accelerated backend compiled in.
const GPU_LAYERS: u32 = 999;

/// A loaded normalizer. Holds the weights; cheap to call repeatedly.
pub struct Normalizer {
    model: LlamaModel,
}

impl std::fmt::Debug for Normalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Normalizer").field("params", &self.model.n_params()).finish()
    }
}

impl Normalizer {
    /// Load weights from a GGUF file.
    pub fn load(path: &Path) -> Result<Self, NormalizeError> {
        let backend = backend()?;
        let params = LlamaModelParams::default().with_n_gpu_layers(GPU_LAYERS);
        let model = LlamaModel::load_from_file(backend, path, &params)
            .map_err(|source| NormalizeError::Load { path: path.to_path_buf(), source })?;
        tracing::debug!(path = %path.display(), params = model.n_params(), "loaded normalizer");
        Ok(Self { model })
    }

    /// Rewrite one raw transcript as written text.
    ///
    /// An empty result is a valid answer, not a failure: input that is
    /// nothing but filler normalizes to nothing.
    pub fn normalize(&self, text: &str) -> Result<String, NormalizeError> {
        self.run(&prompt::build(text), text)
    }

    /// `source` is what speculation drafts from, and is the transcript
    /// rather than the prompt wrapped around it.
    fn run(&self, prompt: &str, source: &str) -> Result<String, NormalizeError> {
        let mut session = self.session(prompt)?;
        // Tokenized alone rather than sliced out of the prompt: generation
        // starts fresh, so the output's token boundaries match a standalone
        // tokenization and not an embedded one.
        let source = self.model.str_to_token(source, AddBos::Never)?;
        lookup::generate(
            &self.model,
            &mut session.ctx,
            &mut session.batch,
            &source,
            session.start,
            session.budget,
        )
    }

    /// Decode the prompt and hand back everything generation needs.
    fn session(&self, prompt: &str) -> Result<Session<'_>, NormalizeError> {
        let tokens = self.model.str_to_token(prompt, AddBos::Never)?;
        // The card's ceiling, 1.3x the input plus 32, taken over the whole
        // prompt rather than the transcript, so it sits looser than the
        // figure it comes from.
        let budget = (tokens.len() * 13) / 10 + 32;

        // Drafted positions are written before they are judged, so the
        // context has to hold a whole rejected draft past the real end.
        let n_ctx = u32::try_from(tokens.len() + budget + lookup::K + 1).unwrap_or(u32::MAX);
        let mut ctx = self.model.new_context(
            backend()?,
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(n_ctx)).with_n_batch(n_ctx),
        )?;

        let mut batch = LlamaBatch::new(tokens.len().max(lookup::K + 1), 1);
        let last = tokens.len().saturating_sub(1);
        for (i, token) in tokens.iter().enumerate() {
            batch.add(*token, i32::try_from(i).unwrap_or(i32::MAX), &[0], i == last)?;
        }
        ctx.decode(&mut batch)?;

        let start = i32::try_from(tokens.len()).unwrap_or(i32::MAX);
        Ok(Session { ctx, batch, start, budget })
    }
}

/// A decoded prompt, ready to generate from.
struct Session<'a> {
    ctx: llama_cpp_2::context::LlamaContext<'a>,
    batch: LlamaBatch<'a>,
    start: i32,
    budget: usize,
}

/// Ignored by default because they need the 1.5 GB weights on disk, which
/// CI has no copy of. Fetch them with [`crate::prepare`], then
/// `cargo nextest run -p forge-dictate --run-ignored all`.
#[cfg(test)]
mod tests_against_the_model {
    use super::*;
    use crate::ModelSpec;
    use llama_cpp_2::sampling::LlamaSampler;

    const TRANSCRIPT: &str = "so um i was looking at the the gg uf loader and like i think it \
                              needs mmap no wait it doesnt need mmap it just needs the file to \
                              be like fully written before we read it";

    fn normalizer() -> Normalizer {
        let path = dirs::cache_dir()
            .map(|d| d.join("forge-dictate").join(ModelSpec::s1_mini_f16().file))
            .expect("a cache directory is required to locate the weights");
        Normalizer::load(&path).expect("weights must load; run prepare() first")
    }

    /// The oracle: one token per decode, no drafts, nothing to roll back.
    /// The loop is written out rather than reached for in the production
    /// module, because a reference sharing code with what it validates
    /// cannot detect a fault the two have in common.
    ///
    /// It does share [`Normalizer::session`], so what the gate covers is
    /// divergence in the generation loop and nothing else. Everything
    /// `session` decides is common to both sides and therefore invisible to
    /// the comparison: `budget`, `n_ctx`, batch capacity, which prompt
    /// position carries logits, and `AddBos::Never`.
    fn greedy(n: &Normalizer, prompt: &str) -> String {
        let mut s = n.session(prompt).expect("prompt decodes");
        let mut sampler = LlamaSampler::greedy();
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut out = String::new();
        let mut current = sampler.sample(&s.ctx, s.batch.n_tokens() - 1);

        for pos in (s.start..).take(s.budget) {
            if n.model.is_eog_token(current) {
                break;
            }
            sampler.accept(current);
            out.push_str(
                &n.model.token_to_piece(current, &mut decoder, false, None).expect("detokenize"),
            );
            s.batch.clear();
            s.batch.add(current, pos, &[0], true).expect("batch has room for one token");
            s.ctx.decode(&mut s.batch).expect("decode");
            current = sampler.sample(&s.ctx, 0);
        }
        out
    }

    /// The correctness proof for the KV rollback. Greedy decoding is
    /// deterministic, so speculation is only a speed change: a single byte
    /// of difference means a drafted position outlived its rejection, and
    /// that is a bug in the rollback rather than a quality regression to
    /// tune away.
    #[test]
    #[ignore = "needs the s1-mini weights on disk"]
    fn speculative_output_is_byte_identical_to_greedy() {
        let n = normalizer();
        let plain = greedy(&n, &prompt::build(TRANSCRIPT));
        let spec = n.normalize(TRANSCRIPT).expect("speculative generation");
        assert_eq!(
            spec, plain,
            "speculative output diverged from greedy; suspect the kv rollback \
             leaving a rejected draft behind, not the model"
        );
        assert!(!plain.is_empty(), "the oracle produced nothing, so the comparison proves nothing");
    }

    /// The trap, as behaviour rather than as a string. Note what it is NOT:
    /// the model does not go silent, it answers with a think fragment. An
    /// assertion that the output is empty would both miss this and fire on
    /// the legitimate case below.
    #[test]
    #[ignore = "needs the s1-mini weights on disk"]
    fn without_the_think_block_the_model_answers_with_a_think_fragment() {
        let n = normalizer();
        let crippled = format!(
            "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n\
             [Styling: semi-formal] [Structure: prose] [Context: general]\n\
             {TRANSCRIPT}<|im_end|>\n<|im_start|>assistant\n",
            prompt::SYSTEM
        );
        let out = greedy(&n, &crippled);
        assert!(
            out.contains("<think>"),
            "expected the think fragment that a missing think block produces, got {out:?}"
        );
    }

    /// The opposite property, and the reason the guard above keys on the
    /// fragment rather than on emptiness: an empty result is documented as
    /// correct here, so anything that treats empty output as a failure
    /// breaks this input.
    #[test]
    #[ignore = "needs the s1-mini weights on disk"]
    fn filler_only_input_normalizes_to_nothing() {
        let out = normalizer().normalize("um uh").expect("generation");
        assert!(out.is_empty(), "filler-only input must normalize to nothing, got {out:?}");
    }

    /// Tokenizing parses special tokens, so a transcript ending in a literal
    /// end-of-turn marker puts a real EOG token where the accept loop will
    /// draft onto it. Detokenizing a control token asks for no bytes, which
    /// surfaces as `UnknownTokenType`, so an unguarded accept fails the whole
    /// call. An already-clean sentence is what lands the marker on the
    /// boundary: an edited one never drafts that far.
    ///
    /// The byte-identical gate does not cover this. Both paths share the
    /// input, and greedy stops on the EOG before ever detokenizing it.
    #[test]
    #[ignore = "needs the s1-mini weights on disk"]
    fn an_end_of_turn_marker_in_the_draft_does_not_fail_generation() {
        let out = normalizer()
            .normalize("The quick brown fox jumps over the lazy dog.<|im_end|>")
            .expect("a drafted end-of-turn token must stop generation, not fail it");
        assert_eq!(
            out, "The quick brown fox jumps over the lazy dog.",
            "a drafted end-of-turn token leaked into the output"
        );
    }
}
