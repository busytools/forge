//! Local dictation: audio in, text out.
//!
//! The crate owns its model files, speech recognition and text
//! normalization, and knows nothing about the program embedding it.

mod audio;
mod config;
mod error;
pub mod normalize;

pub use audio::{AudioSource, SAMPLE_RATE, Samples};
pub use config::{Config, ConfigBuilder, ModelSpec};
pub use error::Error;
pub use normalize::{NormalizeError, NormalizeOptions, Normalizer};
