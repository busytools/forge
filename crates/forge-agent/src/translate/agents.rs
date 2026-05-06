//! Available-agents catalogue parser. Used by App-side
//! `events::sdk_message::apply_available_agents_from_init` to fold a
//! `system/init.agents` payload into the `AvailableAgentsUpdate`
//! event.

use serde_json::Value;

use forge_primitives::AvailableAgent;

/// Mirrors `mapAvailableAgentsFromNames(value)` — agents listed as
/// plain strings (no descriptions). Used when the CLI emits a names-
/// only list.
#[must_use]
pub fn map_available_agents_from_names(value: Option<&Value>) -> Vec<AvailableAgent> {
    let Some(arr) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut by_name: std::collections::BTreeMap<String, AvailableAgent> =
        std::collections::BTreeMap::new();
    for entry in arr {
        let Some(name) = entry.as_str() else { continue };
        let name = name.trim().to_owned();
        if name.is_empty() {
            continue;
        }
        by_name.entry(name.clone()).or_insert_with(|| AvailableAgent {
            name,
            description: String::new(),
            model: None,
        });
    }
    by_name.into_values().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_agents_from_names_dedupes_and_sorts() {
        let v = json!(["b", "a", "a", " ", "c"]);
        let agents = map_available_agents_from_names(Some(&v));
        assert_eq!(agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
    }
}
