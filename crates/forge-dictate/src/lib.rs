//! Local dictation: audio in, text out.
//!
//! The crate owns its model files, speech recognition and text
//! normalization, and knows nothing about the program embedding it.
//!
//! [`prepare`] fetches whatever models a [`Config`] names and checks
//! each one against its recorded size and SHA-256 before anything opens
//! it. A truncated model does not fail at download time; it fails much
//! later, inside a model runtime, as an offset error that reads like a
//! bad build. Verifying first is what turns that into a sentence naming
//! the file.

mod config;
mod error;
mod fetch;

pub use config::{Config, ConfigBuilder, ModelSpec};
pub use error::Error;
pub use fetch::{Progress, prepare};
