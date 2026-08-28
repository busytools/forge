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
//!
//! # The model card describes intent, not measured behaviour
//!
//! Every behavioural claim in it that has been tested here has failed to
//! reproduce: omitting the empty think block returns a `<think>` fragment
//! rather than nothing, `formal` is not `semi-formal` plus expanded
//! contractions, and the card's own three-item `lists` example comes back
//! as prose. The input FORMAT it documents is exact and worth following to
//! the byte; its statements about what the model will do are not. Run the
//! claim before building on it.

mod lookup;
mod prompt;

pub use prompt::{Context, Structure, Styling};

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

    /// A token could not be turned into text. Detokenizing runs bytes
    /// through an incremental decoder, so invalid UTF-8 is not a cause here;
    /// the reachable ones are `UnknownTokenType` for a control token and
    /// `InsufficientBufferSpace`. A control token reaching this point means
    /// the accept loop let one through - see the end-of-turn guard in
    /// `normalize::lookup`.
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
/// Log routing must precede the backend starting, or ggml's Metal
/// device-init block has already reached stderr. Ordering here is documented
/// rather than enforced: swapping the two lines below still compiles.
///
/// `LlamaBackend::void_logs` is not an alternative. It binds llama's sink
/// only, leaving ggml's untouched.
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

/// Per-call settings. [`Default`] is what ships, so a caller that does not
/// care constructs it without naming an axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalizeOptions {
    /// The register to rewrite into.
    pub styling: prompt::Styling,
    /// Whether the model may return a bulleted list.
    pub structure: prompt::Structure,
    /// Destination conventions. `Email` returns multi-line text.
    pub context: prompt::Context,
    /// Draft length for speculative decoding. Decoder tuning rather than a
    /// user-facing choice, and `0` turns speculation off. The optimum is a
    /// property of the text: speculation drafts from the input, so it pays
    /// most when the output barely changes.
    pub k: usize,
    /// Match width for finding a draft. `0` turns speculation off.
    pub ngram: usize,
}

impl Default for NormalizeOptions {
    fn default() -> Self {
        Self {
            styling: prompt::Styling::default(),
            structure: prompt::Structure::default(),
            context: prompt::Context::default(),
            k: lookup::K,
            ngram: lookup::NGRAM,
        }
    }
}

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

    /// Rewrite one raw transcript as written text, at the shipped defaults.
    ///
    /// An empty result is a valid answer, not a failure: input that is
    /// nothing but filler normalizes to nothing.
    pub fn normalize(&self, text: &str) -> Result<String, NormalizeError> {
        self.normalize_with(text, NormalizeOptions::default())
    }

    /// As [`Normalizer::normalize`], with the register and decoder settings
    /// chosen per call. Nothing here is cached against them, so they cost a
    /// string format and can change on every call.
    pub fn normalize_with(
        &self,
        text: &str,
        opts: NormalizeOptions,
    ) -> Result<String, NormalizeError> {
        self.run(&prompt::build(text, opts.styling, opts.structure, opts.context), text, opts)
    }

    /// `source` is what speculation drafts from, and is the transcript
    /// rather than the prompt wrapped around it.
    fn run(
        &self,
        prompt: &str,
        source: &str,
        opts: NormalizeOptions,
    ) -> Result<String, NormalizeError> {
        let mut session = self.session(prompt, opts.k)?;
        // Tokenized alone rather than sliced out of the prompt: generation
        // starts fresh, so the output's token boundaries match a standalone
        // tokenization and not an embedded one.
        let source = self.model.str_to_token(source, AddBos::Never)?;
        lookup::generate(&self.model, &mut session, &source, opts.ngram, opts.k)
    }

    /// Decode the prompt and hand back everything generation needs.
    fn session(&self, prompt: &str, k: usize) -> Result<Session<'_>, NormalizeError> {
        let tokens = self.model.str_to_token(prompt, AddBos::Never)?;
        let Plan { budget, n_ctx, batch_capacity } = plan(tokens.len(), k);

        let mut ctx = self.model.new_context(
            backend()?,
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(n_ctx)).with_n_batch(n_ctx),
        )?;

        let mut batch = LlamaBatch::new(batch_capacity, 1);
        let last = tokens.len().saturating_sub(1);
        for (i, token) in tokens.iter().enumerate() {
            batch.add(*token, i32::try_from(i).unwrap_or(i32::MAX), &[0], i == last)?;
        }
        ctx.decode(&mut batch)?;

        let start = i32::try_from(tokens.len()).unwrap_or(i32::MAX);
        Ok(Session { ctx, batch, start, budget })
    }
}

/// How much room one call needs. Pure arithmetic, lifted out because both
/// the speculative path and its oracle share it, which puts it outside what
/// the byte-identity gate can see.
struct Plan {
    /// Generation stops here whatever the model does.
    budget: usize,
    /// Positions the context must hold.
    n_ctx: u32,
    /// Tokens one batch must hold.
    batch_capacity: usize,
}

/// The card's ceiling is 1.3x the input plus 32, taken over the whole prompt
/// rather than the transcript, so it sits looser than the figure it comes
/// from.
///
/// `n_ctx` covers the highest position ever written: the prompt, then at most
/// `budget` emitted tokens, then a whole `k`-token draft written speculatively
/// past the last accepted one. That highest index is
/// `n_prompt + budget + k - 1`, so the count needed is one more than that and
/// this leaves exactly one slot spare.
///
/// A batch holds either the whole prompt on the first decode or one confirmed
/// token plus a full draft on every later one, so it takes the larger.
fn plan(n_prompt: usize, k: usize) -> Plan {
    let budget = (n_prompt * 13) / 10 + 32;
    Plan {
        budget,
        n_ctx: u32::try_from(n_prompt + budget + k + 1).unwrap_or(u32::MAX),
        batch_capacity: n_prompt.max(k + 1),
    }
}

/// A decoded prompt, ready to generate from.
struct Session<'a> {
    ctx: llama_cpp_2::context::LlamaContext<'a>,
    batch: LlamaBatch<'a>,
    start: i32,
    budget: usize,
}

#[cfg(test)]
mod tests_plan {
    use super::*;

    /// The headroom the whole design rests on, and nothing else checks it:
    /// both the speculative path and its oracle take these numbers from
    /// `plan`, so the byte-identity gate cannot see an error in them.
    ///
    /// One spare slot rather than none is the property. Zero would work
    /// until the first full-length draft, and the failure would arrive as a
    /// decode error deep in a run rather than at the call that sized it.
    #[test]
    fn a_context_holds_the_prompt_a_full_run_and_a_whole_rejected_draft() {
        for n_prompt in [1usize, 2, 69, 100, 512, 4096] {
            for k in [0usize, 1, 2, 64, 512] {
                let p = plan(n_prompt, k);
                // Highest position written: prompt, then budget emitted
                // tokens, then a whole draft past the last accepted one.
                let highest = n_prompt + p.budget + k - 1;
                assert_eq!(
                    u64::from(p.n_ctx),
                    highest as u64 + 2,
                    "n_prompt {n_prompt} k {k}: context must hold every position \
                     written plus exactly one spare"
                );
            }
        }
    }

    /// A batch is filled twice with different shapes: the whole prompt on the
    /// first decode, then one confirmed token plus a full draft on each later
    /// one. Sizing for either alone overflows on the other.
    #[test]
    fn a_batch_holds_the_prompt_and_the_widest_draft() {
        for n_prompt in [1usize, 69, 4096] {
            for k in [0usize, 64, 8192] {
                let c = plan(n_prompt, k).batch_capacity;
                let widest_generation_batch = k + 1;
                assert!(
                    c >= n_prompt,
                    "n_prompt {n_prompt} k {k}: batch too small for the prompt decode"
                );
                assert!(
                    c >= widest_generation_batch,
                    "n_prompt {n_prompt} k {k}: batch too small for a confirmed token \
                     plus a full draft"
                );
            }
        }
    }

    /// Output length tracks input length, so the ceiling has to scale with
    /// it rather than sitting at a constant.
    #[test]
    fn the_budget_grows_with_the_prompt() {
        assert!(
            plan(1000, 0).budget > plan(100, 0).budget,
            "a longer prompt must be allowed a longer output"
        );
    }
}

/// Ignored by default because they need the 1.5 GB weights on disk, which
/// CI has no copy of. Fetch them with [`crate::prepare`], then
/// `cargo nextest run -p forge-dictate --run-ignored all`.
///
/// # What a green CI run does not tell you
///
/// Everything below is enforced only when someone runs it locally, so the
/// KV rollback, the accept loop, the end-of-turn guard and the sampling
/// index are unenforced in CI: severing any of them passes the whole
/// automated suite. The byte-identity property is a claim about a local run
/// rather than one the repository holds. `tests_plan` is model-free and
/// does hold in CI, but it covers the sizing arithmetic only.
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
    fn greedy(n: &Normalizer, prompt: &str, k: usize) -> String {
        let mut s = n.session(prompt, k).expect("prompt decodes");
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
    ///
    /// The property is claimed for **every** `(ngram, k)`; the pairs below
    /// are the sampled witnesses, not the extent of the claim, and `k = 0`
    /// covers the degenerate no-speculation case. **Changing [`lookup::K`]
    /// means adding a pair here**, or the shipped setting stops being one of
    /// the witnesses.
    ///
    /// Compared against the oracle rather than against another `k`: two
    /// speculative runs share the drafting loop, so they would agree on a
    /// fault they both have.
    #[test]
    #[ignore = "needs the s1-mini weights on disk"]
    fn speculative_output_is_byte_identical_to_greedy() {
        let n = normalizer();
        for (ngram, k) in [(2, 64), (1, 4), (3, 16), (2, 0)] {
            let opts = NormalizeOptions { k, ngram, ..Default::default() };
            let plain = greedy(
                &n,
                &prompt::build(TRANSCRIPT, opts.styling, opts.structure, opts.context),
                k,
            );
            let spec = n.normalize_with(TRANSCRIPT, opts).expect("speculative generation");
            assert_eq!(
                spec, plain,
                "ngram {ngram} k {k} diverged from greedy; suspect the kv rollback \
                 leaving a rejected draft behind, not the model"
            );
            assert!(
                !plain.is_empty(),
                "the oracle produced nothing at ngram {ngram} k {k}, so the comparison \
                 proves nothing"
            );
        }
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
        let out = greedy(&n, &crippled, lookup::K);
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
