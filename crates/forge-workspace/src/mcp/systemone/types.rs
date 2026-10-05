//! Argument shapes for the systemone tools.

use std::collections::BTreeMap;

/// `systemone__ask_noul` arguments.
#[derive(serde::Deserialize)]
pub(crate) struct NoulArgs {
    pub state: serde_json::Value,
    pub instructions: serde_json::Value,
    /// The keys stay exactly `true` and `false`; the values are any JSON.
    pub criteria: Option<BTreeMap<String, serde_json::Value>>,
}

/// `systemone__ask_choice` arguments.
#[derive(serde::Deserialize)]
pub(crate) struct ChoiceArgs {
    pub state: serde_json::Value,
    pub instructions: serde_json::Value,
    /// Option names mapped to any JSON: a string, an object, an array or null.
    pub criteria: BTreeMap<String, serde_json::Value>,
}

/// `systemone__ask_score` arguments.
#[derive(serde::Deserialize)]
pub(crate) struct ScoreArgs {
    pub state: serde_json::Value,
    pub instructions: serde_json::Value,
    /// The ordered levels, lowest first; each level is any JSON but null.
    pub criteria: Vec<serde_json::Value>,
}
