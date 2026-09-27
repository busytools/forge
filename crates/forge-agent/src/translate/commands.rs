//! Available-commands catalogue parser, read by the workspace's
//! session task and by the TUI's own message walker.

use serde_json::Value;

use forge_primitives::AvailableCommand;

/// Parse a `slash_commands` / `commands` array into `AvailableCommand`s.
/// Entries are either bare name strings (the `system/init`
/// `slash_commands` shape) or `{name, description, argumentHint}`
/// objects (the `commands_changed` shape); both flow through here so
/// init and the live refresh share one boundary. Non-string / nameless
/// entries are skipped; an empty `argumentHint` collapses to `None`.
pub fn map_available_commands_from_json(arr: &[Value]) -> Vec<AvailableCommand> {
    arr.iter()
        .filter_map(|entry| {
            if let Some(name) = entry.as_str() {
                if name.is_empty() {
                    return None;
                }
                return Some(AvailableCommand {
                    name: name.to_owned(),
                    description: String::new(),
                    input_hint: None,
                });
            }
            let obj = entry.as_object()?;
            let name = obj.get("name")?.as_str().filter(|s| !s.is_empty())?.to_owned();
            let description =
                obj.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
            let input_hint = obj
                .get("argumentHint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            Some(AvailableCommand { name, description, input_hint })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::map_available_commands_from_json;
    use forge_primitives::AvailableCommand;
    use serde_json::{Value, json};

    fn parse(entries: &Value) -> Vec<AvailableCommand> {
        map_available_commands_from_json(entries.as_array().expect("an array"))
    }

    /// The `system/init` shape: bare names, no descriptions, which is
    /// what the CLI sends on the first frame of a turn.
    #[test]
    fn bare_name_entries_map_to_commands_without_descriptions() {
        let commands = parse(&json!(["/help", "memory"]));

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/help", "memory"], "both entries survive, in wire order");
        assert!(commands.iter().all(|c| c.description.is_empty()), "and carry no description");
        assert!(commands.iter().all(|c| c.input_hint.is_none()), "nor an input hint");
    }

    /// The `commands_changed` shape: objects with a description and an
    /// optional argument hint.
    #[test]
    fn object_entries_carry_their_description_and_argument_hint() {
        let commands = parse(&json!([
            {"name": "/model", "description": "Pick a model", "argumentHint": "<id>"},
        ]));

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].description, "Pick a model");
        assert_eq!(commands[0].input_hint.as_deref(), Some("<id>"));
    }

    /// An absent or empty hint is no hint at all: `Some("")` would draw
    /// an empty argument slot beside the command.
    #[test]
    fn an_empty_argument_hint_collapses_to_none() {
        let commands = parse(&json!([
            {"name": "/clear", "description": "Clear", "argumentHint": ""},
            {"name": "/compact"},
        ]));

        assert_eq!(commands.len(), 2);
        assert!(commands[0].input_hint.is_none(), "an empty hint is no hint");
        assert!(commands[1].input_hint.is_none(), "and a missing one is the same");
        assert!(commands[1].description.is_empty(), "a missing description is empty, not absent");
    }

    /// A nameless or non-string entry would draw a blank row in the
    /// dropdown, so it is dropped rather than carried.
    #[test]
    fn entries_without_a_name_are_dropped() {
        let commands = parse(&json!([
            "",
            {"description": "no name here"},
            {"name": ""},
            42,
            "/kept",
        ]));

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/kept"], "only the entry with a name survives: {names:?}");
    }
}
