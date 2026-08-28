//! Local dictation: audio in, text out.
//!
//! The crate owns its model files, speech recognition and text
//! normalization, and knows nothing about the program embedding it.

mod audio;
mod error;

pub use audio::{AudioSource, SAMPLE_RATE, Samples};
pub use error::Error;
