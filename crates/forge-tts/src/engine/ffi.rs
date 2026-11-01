//! llama.cpp's audio-generation pipeline, which the safe binding does not wrap.
#![allow(unsafe_code, reason = "llama.cpp's audio-generation API is a C ABI with no safe wrapper")]

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use llama_cpp_sys_2 as sys;

use crate::{Chunk, Error, SpeakRequest};

/// The backbone's own defaults for sampling semantic codes.
const TOP_K: i32 = 40;
const TOP_P: f32 = 0.95;
const MIN_P: f32 = 0.05;
const TEMPERATURE: f32 = 0.8;

/// `LLAMA_DEFAULT_SEED`: the value llama.cpp reads as "pick a random seed".
const RANDOM_SEED: u32 = u32::MAX;

/// The tool's own ceiling on generated frames, about forty seconds of audio.
const MAX_FRAMES: u32 = 512;

/// One loaded pipeline: the backbone, its codec projector, and the context
/// both decode through. Owned by the render thread, one utterance at a time.
pub(super) struct Core {
    model: *mut sys::llama_model,
    projector: *mut sys::mtmd_context,
    context: *mut sys::llama_context,
    sample_rate: u32,
}

impl Core {
    pub(super) fn load(model: &Path, projector: &Path) -> Result<Self, Error> {
        let mut core = Self {
            model: ptr::null_mut(),
            projector: ptr::null_mut(),
            context: ptr::null_mut(),
            sample_rate: 0,
        };

        // Idempotent: forge-dictate narrates its own first call.
        unsafe { sys::llama_backend_init() };

        let model_path = utf8(model)?;
        let name =
            CString::new(model_path).map_err(|_| Error::Load("model path has a NUL".to_owned()))?;
        core.model = unsafe {
            sys::llama_model_load_from_file(name.as_ptr(), sys::llama_model_default_params())
        };
        if core.model.is_null() {
            return Err(Error::Load(format!("llama.cpp could not load {model_path}")));
        }

        let mut params = unsafe { sys::mtmd_context_params_default() };
        params.print_timings = false;
        let projector_path = utf8(projector)?;
        let name = CString::new(projector_path)
            .map_err(|_| Error::Load("projector path has a NUL".to_owned()))?;
        core.projector = unsafe { sys::mtmd_init_from_file(name.as_ptr(), core.model, params) };
        if core.projector.is_null() {
            return Err(Error::Load(format!("llama.cpp could not load {projector_path}")));
        }
        let info = unsafe { sys::mtmd_gen_audio_get_info(core.projector) };
        if info.type_ == sys::MTMD_GEN_AUDIO_TYPE_NONE {
            return Err(Error::Load(format!("{projector_path} carries no audio codec")));
        }
        core.sample_rate = u32::try_from(info.sample_rate).unwrap_or(0);

        let mut context = unsafe { sys::llama_context_default_params() };
        // The model's own trained window, and per-token embeddings for the
        // backbone state each generated frame is built from.
        context.n_ctx = 0;
        context.embeddings = true;
        core.context = unsafe { sys::llama_init_from_model(core.model, context) };
        if core.context.is_null() {
            return Err(Error::Load("llama.cpp could not create a context".to_owned()));
        }

        Ok(core)
    }

    pub(super) fn render(&mut self, request: &SpeakRequest) -> Result<Chunk, Error> {
        let text = CString::new(request.text.as_str())
            .map_err(|_| Error::Render("the text contains a NUL".to_owned()))?;
        let language = option_cstring(request.language.as_deref())?;
        let voice = option_cstring(request.speaker.as_deref())?;

        let n_batch =
            i32::try_from(unsafe { sys::llama_n_batch(self.context) }).unwrap_or(i32::MAX);

        // A frame is a full decode at the current position, and the previous
        // utterance stopped partway through the window.
        unsafe {
            sys::llama_memory_clear(sys::llama_get_memory(self.context), true);
        }

        let sampler = Sampler::new();
        let audio = Gen::new(self.context, self.projector)?;

        let mut input = unsafe { sys::mtmd_helper_gen_audio_inp_default() };
        input.stream = false;
        input.seq_id = 0;
        input.prompt = text.as_ptr();
        input.prompt_len = request.text.len();
        input.n_past = 0;
        input.out_mode = sys::MTMD_HELPER_GEN_AUDIO_OUT_AUTO;
        input.speaker_ref = ptr::null_mut();
        input.lang = language.as_ref().map_or(ptr::null(), |value| value.as_ptr());
        input.voice = voice.as_ref().map_or(ptr::null(), |value| value.as_ptr());
        input.top_k = TOP_K;
        input.top_p = TOP_P;
        input.seed = RANDOM_SEED;
        input.out_type = sys::MTMD_HELPER_GEN_AUDIO_OUTTYPE_PCM;
        if unsafe { sys::mtmd_helper_gen_audio_set_input(audio.raw, &raw const input) } != 0 {
            return Err(Error::Render("the text could not be turned into a prompt".to_owned()));
        }

        loop {
            let remaining = unsafe { sys::mtmd_helper_gen_audio_step_prompt(audio.raw, n_batch) };
            if remaining < 0 {
                return Err(Error::Render("the prompt failed to decode".to_owned()));
            }
            if remaining == 0 {
                break;
            }
        }

        let mut code = sampler.sample(self.context);
        sampler.accept(code);
        let mut state: *const f32 = unsafe { sys::llama_get_embeddings_ith(self.context, -1) };
        if state.is_null() {
            return Err(Error::Render("the backbone produced no state".to_owned()));
        }

        for _ in 0..MAX_FRAMES {
            let mut next: *const f32 = ptr::null();
            let mut stop = false;
            let status = unsafe {
                sys::mtmd_helper_gen_audio_step_gen(
                    audio.raw,
                    code,
                    state,
                    &raw mut next,
                    &raw mut stop,
                )
            };
            if status != 0 {
                return Err(Error::Render(format!("a frame failed to decode ({status})")));
            }
            if stop || next.is_null() {
                break;
            }
            state = next;
            code = sampler.sample(self.context);
            sampler.accept(code);
        }

        let mut rate = i32::try_from(self.sample_rate).unwrap_or(0);
        let mut data: *const std::os::raw::c_char = ptr::null();
        let mut len = 0_usize;
        let mut n_samples = 0_i64;
        let status = unsafe {
            sys::mtmd_helper_gen_audio_get_output(
                audio.raw,
                &raw mut rate,
                &raw mut data,
                &raw mut len,
                &raw mut n_samples,
            )
        };
        if status != 0 {
            return Err(Error::Render("the audio could not be read back".to_owned()));
        }
        let count = usize::try_from(n_samples).unwrap_or(0);
        let pcm = if data.is_null() || count == 0 {
            Vec::new()
        } else {
            // SAFETY: a non-streaming read hands back the whole clip as f32
            // samples, valid until the next call on this context.
            let floats = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count) };
            to_pcm(floats)
        };

        Ok(Chunk { seq: 0, sample_rate: u32::try_from(rate).unwrap_or(self.sample_rate), pcm })
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe {
            if !self.context.is_null() {
                sys::llama_free(self.context);
            }
            if !self.projector.is_null() {
                sys::mtmd_free(self.projector);
            }
            if !self.model.is_null() {
                sys::llama_model_free(self.model);
            }
        }
    }
}

/// The audio-generation context; freed on every exit path.
struct Gen {
    raw: *mut sys::mtmd_helper_gen_audio,
}

impl Gen {
    fn new(
        context: *mut sys::llama_context,
        projector: *mut sys::mtmd_context,
    ) -> Result<Self, Error> {
        let raw = unsafe { sys::mtmd_helper_gen_audio_init(context, projector) };
        if raw.is_null() {
            return Err(Error::Render("audio generation could not start".to_owned()));
        }
        Ok(Self { raw })
    }
}

impl Drop for Gen {
    fn drop(&mut self) {
        unsafe { sys::mtmd_helper_gen_audio_free(self.raw) };
    }
}

/// The backbone's semantic-code sampler.
struct Sampler {
    raw: *mut sys::llama_sampler,
}

impl Sampler {
    fn new() -> Self {
        unsafe {
            let chain = sys::llama_sampler_chain_init(sys::llama_sampler_chain_default_params());
            sys::llama_sampler_chain_add(chain, sys::llama_sampler_init_top_k(TOP_K));
            sys::llama_sampler_chain_add(chain, sys::llama_sampler_init_top_p(TOP_P, 1));
            sys::llama_sampler_chain_add(chain, sys::llama_sampler_init_min_p(MIN_P, 1));
            sys::llama_sampler_chain_add(chain, sys::llama_sampler_init_temp(TEMPERATURE));
            sys::llama_sampler_chain_add(chain, sys::llama_sampler_init_dist(RANDOM_SEED));
            Self { raw: chain }
        }
    }

    fn sample(&self, context: *mut sys::llama_context) -> sys::llama_token {
        unsafe { sys::llama_sampler_sample(self.raw, context, -1) }
    }

    fn accept(&self, token: sys::llama_token) {
        unsafe { sys::llama_sampler_accept(self.raw, token) };
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe { sys::llama_sampler_free(self.raw) };
    }
}

/// The clip as the rest of forge carries it: clamped and scaled to i16.
fn to_pcm(samples: &[f32]) -> Vec<i16> {
    samples.iter().map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16).collect()
}

fn utf8(path: &Path) -> Result<&str, Error> {
    path.to_str().ok_or_else(|| Error::Load(format!("path is not UTF-8: {}", path.display())))
}

fn option_cstring(value: Option<&str>) -> Result<Option<CString>, Error> {
    value
        .map(|text| {
            CString::new(text).map_err(|_| Error::Render("a voice setting has a NUL".to_owned()))
        })
        .transpose()
}
