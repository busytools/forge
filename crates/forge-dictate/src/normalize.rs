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

/// The architectures this stage's generator can run: llama.cpp's causal text
/// models.
///
/// **A guard, not a preference.** An encoder or encoder-decoder gguf loads
/// and then aborts the process mid-decode (`ggml_abort` on a cross-attention
/// input a causal decode never sets), and an abort cannot be caught, so the
/// check is by name and it happens before the load. The cleanup feed reads
/// this same list to decide what it offers, so a candidate and the file the
/// load accepts cannot disagree.
pub const CAUSAL_ARCHS: [&str; 9] =
    ["qwen2", "qwen3", "llama", "gemma", "gemma2", "gemma3", "mistral", "phi2", "phi3"];

use std::io::{Read as _, Seek as _};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
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

    /// The file is not a model this stage runs, refused BEFORE the load.
    ///
    /// **The check has to happen here, because the alternative is a process
    /// abort.** llama.cpp loads an encoder-decoder gguf without complaint and
    /// then asserts inside the first decode on the cross-attention input a
    /// causal decode never sets - `llm_graph_input_attn_cross::set_input`,
    /// measured on a t5 build - and a ggml assert aborts rather than
    /// returning, so nothing can catch it after the load. The architecture is
    /// read out of the file's own header first, and a file that declares none
    /// is refused on the same grounds: an undeclared arch is a promise nobody
    /// can make.
    #[error(
        "{} is {arch}, and the normalizer runs {} only",
        path.display(),
        CAUSAL_ARCHS.join(", ")
    )]
    NotCausal { path: PathBuf, arch: String },

    /// A context could not be created for the loaded weights.
    #[error("could not create an inference context: {0}")]
    Context(#[from] llama_cpp_2::LlamaContextLoadError),

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
    ///
    /// The architecture is checked against [`CAUSAL_ARCHS`] BEFORE the load -
    /// see [`NormalizeError::NotCausal`] for why that order is the whole
    /// point of the check.
    pub fn load(path: &Path) -> Result<Self, NormalizeError> {
        let arch = architecture(path);
        if !arch.as_deref().is_some_and(|arch| CAUSAL_ARCHS.contains(&arch)) {
            return Err(NormalizeError::NotCausal {
                path: path.to_path_buf(),
                arch: match arch {
                    Some(arch) => format!("a {arch} model"),
                    None => "an architecture this file does not declare".to_owned(),
                },
            });
        }
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
        self.run(&prompt::for_options(text, opts), text, opts)
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
        let source = self.model.vocab().tokenize(source.as_bytes(), false, true);
        lookup::generate(&self.model, &mut session, &source, opts.ngram, opts.k)
    }

    /// Decode the prompt and hand back everything generation needs.
    fn session(&self, prompt: &str, k: usize) -> Result<Session<'_>, NormalizeError> {
        let tokens = self.model.vocab().tokenize(prompt.as_bytes(), false, true);
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

/// Decode one token's bytes through a decoder the whole generation reuses.
/// The decoder must persist across tokens: a piece can end mid-character,
/// and a fresh decoder would replace both halves.
fn decode_piece(decoder: &mut encoding_rs::Decoder, bytes: &[u8]) -> String {
    let mut piece = String::with_capacity(
        decoder.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len().saturating_mul(3)),
    );
    let mut rest = bytes;
    loop {
        let (result, read, _) = decoder.decode_to_string(rest, &mut piece, false);
        if matches!(result, encoding_rs::CoderResult::InputEmpty) {
            return piece;
        }
        piece.reserve(rest.len().saturating_mul(3).max(8));
        rest = &rest[read..];
    }
}

/// How much room one call needs.
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

/// The architecture a gguf declares, read from the file's own header.
///
/// **Read BEFORE the load, and that order is the point.** llama.cpp loads an
/// encoder-decoder gguf without complaint and asserts inside the first
/// decode, and a ggml assert aborts the process - so the file has to be
/// refused while refusing is still possible. `None` when the header is not a
/// GGUF one, is malformed, or declares nothing, which the caller refuses on
/// the same grounds as an architecture it cannot run.
fn architecture(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0_u8; 24];
    file.read_exact(&mut head).ok()?;
    if &head[0..4] != b"GGUF" {
        return None;
    }
    let pairs = u64::from_le_bytes(head[16..24].try_into().ok()?);
    // A header's own count is a claim like any other: the loop ends at the
    // count, at the first malformed read, and at the file's end.
    for _ in 0..pairs.min(4096) {
        let key = read_string(&mut file)?;
        let kind = read_u32(&mut file)?;
        if key == "general.architecture" {
            return (kind == GGUF_STRING).then(|| read_string(&mut file)).flatten();
        }
        skip_value(&mut file, kind, 0)?;
    }
    None
}

/// The format's own type tag for a string value.
const GGUF_STRING: u32 = 8;

fn read_u32(file: &mut std::fs::File) -> Option<u32> {
    let mut bytes = [0_u8; 4];
    file.read_exact(&mut bytes).ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u64(file: &mut std::fs::File) -> Option<u64> {
    let mut bytes = [0_u8; 8];
    file.read_exact(&mut bytes).ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// One length-prefixed utf-8 value, bounded so a corrupt length cannot ask
/// for an allocation nobody has.
fn read_string(file: &mut std::fs::File) -> Option<String> {
    let length = usize::try_from(read_u64(file)?).ok()?;
    if length > 1 << 20 {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// Step over one metadata value, by the format's own type table. Anything
/// the table does not name ends the walk, which the caller answers with the
/// same refusal as a malformed header.
fn skip_value(file: &mut std::fs::File, kind: u32, depth: u32) -> Option<()> {
    if depth > 4 {
        return None;
    }
    let width = match kind {
        0 | 1 | 7 => 1_u64,
        2..=3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        GGUF_STRING => {
            read_string(file)?;
            return Some(());
        }
        9 => {
            let element = read_u32(file)?;
            let count = read_u64(file)?;
            if count > 1 << 24 {
                return None;
            }
            let element_width = match element {
                0 | 1 | 7 => Some(1_u64),
                2..=3 => Some(2),
                4..=6 => Some(4),
                10..=12 => Some(8),
                _ => None,
            };
            match element_width {
                Some(element_width) => {
                    let bytes = i64::try_from(count.checked_mul(element_width)?).ok()?;
                    file.seek(std::io::SeekFrom::Current(bytes)).ok()?;
                }
                None => {
                    for _ in 0..count {
                        skip_value(file, element, depth + 1)?;
                    }
                }
            }
            return Some(());
        }
        _ => return None,
    };
    file.seek(std::io::SeekFrom::Current(i64::try_from(width).ok()?)).ok()?;
    Some(())
}

#[cfg(test)]
mod tests_architecture {
    use super::*;

    /// A gguf header carrying one metadata pair, built by the format's own
    /// table: magic, version, no tensors, one pair, then the pair.
    fn header(key: &str, kind: u32, value: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GGUF");
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(value);
        bytes
    }

    /// A string value as the format writes one.
    fn string(text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(text.len() as u64).to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    /// **The architecture is read before anything loads**, because the
    /// failure it guards against is a process abort inside the decode: a t5
    /// build loads happily and dies on the first cross-attention input.
    #[test]
    fn the_architecture_is_read_from_the_header_and_everything_else_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        let t5 = dir.path().join("t5.gguf");
        std::fs::write(&t5, header("general.architecture", GGUF_STRING, &string("t5"))).unwrap();
        assert_eq!(architecture(&t5).as_deref(), Some("t5"));

        let qwen = dir.path().join("qwen.gguf");
        std::fs::write(&qwen, header("general.architecture", GGUF_STRING, &string("qwen3")))
            .unwrap();
        assert_eq!(architecture(&qwen).as_deref(), Some("qwen3"));

        // A file that is not a gguf, a header truncated mid-pair, and a
        // pair that is not the architecture: all read as no declaration.
        let other = dir.path().join("weights");
        std::fs::write(&other, b"weights").unwrap();
        assert_eq!(architecture(&other), None);

        let truncated = dir.path().join("truncated.gguf");
        let full = header("general.architecture", GGUF_STRING, &string("qwen3"));
        std::fs::write(&truncated, &full[..20]).unwrap();
        assert_eq!(architecture(&truncated), None);

        let unrelated = dir.path().join("unrelated.gguf");
        std::fs::write(&unrelated, header("general.name", GGUF_STRING, &string("x"))).unwrap();
        assert_eq!(architecture(&unrelated), None);
    }

    /// **The load refuses a non-causal architecture by name, and it is the
    /// load's own refusal that keeps the process alive.** A t5 file loads
    /// happily and aborts inside ggml's cross-attention - an abort nothing
    /// can catch - so the guard is the header read in front of the loader,
    /// and deleting it would leave this the only test that notices.
    #[test]
    fn the_load_refuses_a_non_causal_architecture_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let t5 = dir.path().join("grammar-t5.gguf");
        std::fs::write(&t5, header("general.architecture", GGUF_STRING, &string("t5"))).unwrap();

        let refusal = Normalizer::load(&t5).expect_err("t5 is not a generator llama.cpp runs");
        assert!(matches!(refusal, NormalizeError::NotCausal { .. }), "got: {refusal:?}");
        assert!(
            refusal.to_string().contains("t5"),
            "the refusal names the architecture it read, got: {refusal}"
        );
    }
}

#[cfg(test)]
mod tests_plan {
    use super::*;

    /// One spare slot rather than none. Zero would work until the first
    /// full-length draft, and the failure would surface as a decode error
    /// deep in a run rather than at the call that sized it.
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
/// Everything below needs the weights, so the KV rollback, the accept loop,
/// the end-of-turn guard and the sampling index are unenforced in CI, and
/// byte-identity holds of a local run rather than of the repository.
/// `tests_plan` is model-free and does hold, covering the sizing arithmetic
/// only.
///
/// The `budget` ceiling is unenforced even locally, because both paths stop
/// on an end-of-turn token first. It scales on the whole prompt while output
/// tracks only the transcript, so the scaffold alone holds the floor above
/// any run.
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
    /// position carries logits, and the tokenizer's `add_special = false`.
    fn greedy(n: &Normalizer, prompt: &str, k: usize) -> String {
        let mut s = n.session(prompt, k).expect("prompt decodes");
        let mut sampler = LlamaSampler::greedy();
        let vocab = n.model.vocab();
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut out = String::new();
        let mut current = sampler.sample(&s.ctx, s.batch.n_tokens() - 1);

        for pos in (s.start..).take(s.budget) {
            if vocab.is_eog(current) {
                break;
            }
            sampler.accept(current);
            out.push_str(&decode_piece(&mut decoder, &vocab.token_to_piece(current, false, None)));
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
    /// covers the degenerate no-speculation case. The first pair tracks the
    /// shipped constants so it cannot drift away from them.
    ///
    /// Compared against the oracle rather than against another `k`: two
    /// speculative runs share the drafting loop, so they would agree on a
    /// fault they both have.
    #[test]
    #[ignore = "needs the S1-mini weights on disk"]
    fn speculative_output_is_byte_identical_to_greedy() {
        let n = normalizer();
        for (ngram, k) in [(lookup::NGRAM, lookup::K), (1, 4), (3, 16), (2, 0)] {
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
    #[ignore = "needs the S1-mini weights on disk"]
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
    #[ignore = "needs the S1-mini weights on disk"]
    fn filler_only_input_normalizes_to_nothing() {
        let out = normalizer().normalize("um uh").expect("generation");
        assert!(out.is_empty(), "filler-only input must normalize to nothing, got {out:?}");
    }

    /// Tokenizing parses special tokens, so a transcript ending in a literal
    /// end-of-turn marker puts a real EOG token where the accept loop will
    /// draft onto it. An unguarded accept decodes to no bytes, so it fails
    /// nothing and instead runs one step past the marker into whatever the
    /// model emits next. An already-clean sentence is what lands the marker
    /// on the boundary: an edited one never drafts that far.
    ///
    /// The byte-identical gate does not cover this. Both paths share the
    /// input, and greedy stops on the EOG before ever detokenizing it.
    #[test]
    #[ignore = "needs the S1-mini weights on disk"]
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
