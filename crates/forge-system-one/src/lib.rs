//! System One decision-model client: typed questions in, probabilities
//! out. One endpoint and one model per `[systemone]` config section.

pub mod client;
pub mod config;
pub mod error;
pub mod wire;

pub use client::SystemOneClient;
pub use config::{DEFAULT_TIMEOUT_MS, SystemOneConfig, SystemOneSection, systemone_url};
pub use error::SystemOneError;
pub use wire::{Answer, AskOutcome, NoulCriteria, Question, Usage, validate_answer};
