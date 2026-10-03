//! System One decision-model client: typed questions in, probabilities
//! out. One endpoint and one model per `[systemone]` config section.

pub mod config;

pub use config::{DEFAULT_TIMEOUT_MS, SystemOneConfig, SystemOneSection, systemone_url};
