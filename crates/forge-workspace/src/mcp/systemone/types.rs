//! Argument shapes for the systemone tools.

use std::collections::BTreeMap;

/// `systemone__ask_noul` arguments.
#[derive(serde::Deserialize)]
pub(crate) struct NoulArgs {
    pub state: serde_json::Value,
    pub instructions: String,
    pub criteria: Option<BTreeMap<String, String>>,
}

/// `systemone__ask_choice` arguments.
#[derive(serde::Deserialize)]
pub(crate) struct ChoiceArgs {
    pub state: serde_json::Value,
    pub instructions: String,
    pub criteria: BTreeMap<String, Option<String>>,
}

/// `systemone__ask_score` arguments.
#[derive(serde::Deserialize)]
pub(crate) struct ScoreArgs {
    pub state: serde_json::Value,
    pub instructions: String,
    pub criteria: Vec<String>,
}
