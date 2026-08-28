//! Repairs raw speech-recognition output into written text.
//!
//! Punctuation, capitalization, filler removal, spoken numbers and dates
//! rendered in written form, and self-corrections resolved to whatever the
//! speaker landed on. Text in, text out: this stage never sees audio.

mod prompt;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
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
        self.run(&prompt::build(text))
    }

    fn run(&self, prompt: &str) -> Result<String, NormalizeError> {
        let tokens = self.model.str_to_token(prompt, AddBos::Never)?;
        // The card's ceiling: output length tracks input length closely.
        let budget = (tokens.len() * 13) / 10 + 32;

        let n_ctx = u32::try_from(tokens.len() + budget).unwrap_or(u32::MAX);
        let mut ctx = self.model.new_context(
            backend()?,
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(n_ctx)).with_n_batch(n_ctx),
        )?;

        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        let last = tokens.len().saturating_sub(1);
        for (i, token) in tokens.iter().enumerate() {
            batch.add(*token, i32::try_from(i).unwrap_or(i32::MAX), &[0], i == last)?;
        }
        ctx.decode(&mut batch)?;

        let mut sampler = LlamaSampler::greedy();
        let mut out = String::new();
        // Held across the whole generation: one token can carry part of a
        // multi-byte sequence, and a per-token decoder turns that into
        // replacement characters.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let start = i32::try_from(tokens.len()).unwrap_or(i32::MAX);
        let mut current = sampler.sample(&ctx, batch.n_tokens() - 1);

        for pos in (start..).take(budget) {
            if self.model.is_eog_token(current) {
                break;
            }
            sampler.accept(current);
            out.push_str(&self.model.token_to_piece(current, &mut decoder, false, None)?);

            batch.clear();
            batch.add(current, pos, &[0], true)?;
            ctx.decode(&mut batch)?;
            current = sampler.sample(&ctx, 0);
        }

        Ok(out)
    }
}

/// Offload everything; llama.cpp clamps this to the layers that exist and
/// silently does nothing without an accelerated backend compiled in.
const GPU_LAYERS: u32 = 999;
