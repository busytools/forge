# Vendored llama.cpp bindings

`llama-cpp-2` and `llama-cpp-sys-2` here are crates.io 0.1.160, carried as path
dependencies through `[patch.crates-io]` in the workspace manifest. They exist
because the newest published pair vendors llama.cpp v0.6.0, and forge's TTS
engine needs v0.8.0.

## What differs from the published crates

- `llama-cpp-sys-2/llama.cpp/` is the ggml-org/llama.cpp `v0.8.0` tag, trimmed
  to the directories the published crate ships (`ggml`, `include`, `common`,
  `src`, `tools/mtmd`, `vendor`, `cmake`, `pocs`, the conversion script and the
  top-level CMake files). The other `tools/` directories are removed.
- `llama-cpp-2/src/timing.rs` sets `n_reallocs_same` and `n_reallocs_diff` to
  zero in the hand-built timing literal. llama.cpp added both fields in v0.7.0,
  and the safe binding builds that struct without them, so it does not compile
  against v0.8.0 unpatched.
- `llama-cpp-2/src/lib.rs` allows `dead_code` and `unused_imports`. The crate
  has items gated behind features forge does not enable, and a path dependency
  is built without the lint cap cargo applies to registry crates.

## Why v0.8.0

The Qwen3-TTS graph hands its attention mask to `ggml_flash_attn_ext` as F32
before v0.7.0. On Metal the kernel reads the mask as `half` and the backend's
`supports_op` does not reject an F32 mask, so flash attention stays enabled and
renders against the wrong mask. v0.8.0 casts the mask to F16 when flash
attention is on (`mtmd: add build_inp_attn_mask`, ggml-org/llama.cpp #30259).

## Updating

Replace `llama-cpp-sys-2/llama.cpp/` with the new tag's same directory set, then
build. Drop the `timing.rs` delta once the published safe crate vendors a
llama.cpp release that has the two fields.
