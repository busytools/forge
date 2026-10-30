//! The comment-preserving writer behind the settings editor.
//!
//! An edit names a catalogue pattern and the concrete path it fills; the
//! pattern says where the item sits, so a display path with dots in a name
//! is never re-parsed as TOML. One pass applies every edit to the current
//! text, and the caller validates the candidate with the boot loader before
//! it replaces the file.

use forge_primitives::settings::{
    EditKind, SettingsDocument, SettingsEditOp, SettingsEditRequest, SettingsKey,
};
use regex::Regex;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// Why an edit could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteError {
    /// The loader-facing words, and the key to anchor them at where one is
    /// known.
    pub message: String,
    pub key_path: Option<String>,
}

fn invalid(message: impl Into<String>, key_path: Option<String>) -> WriteError {
    WriteError { message: message.into(), key_path }
}

/// Apply every edit to the current text.
pub(crate) fn apply(
    current: &str,
    edits: &[SettingsEditRequest],
    document: &SettingsDocument,
) -> Result<String, WriteError> {
    let mut doc: DocumentMut = current
        .parse()
        .map_err(|error| invalid(format!("the current file does not parse: {error}"), None))?;
    // **A rename is applied last.** Locating an entry by its pre-edit name
    // (an org's, a project's, an account's) fails once that name moved, so a
    // batch that renames and edits the same entry applies the edits first.
    let (renames, rest): (Vec<&SettingsEditRequest>, Vec<&SettingsEditRequest>) =
        edits.iter().partition(|edit| {
            edit.op == SettingsEditOp::Set && identity_pattern(edit, document).is_some()
        });
    for edit in rest {
        apply_one(&mut doc, edit, document)?;
    }
    for edit in renames {
        apply_one(&mut doc, edit, document)?;
    }
    let text = doc.to_string();
    // toml_edit renders `\n`; a CRLF file comes back as it went in.
    Ok(if current.contains("\r\n") && !text.contains('\r') {
        text.replace('\n', "\r\n")
    } else {
        text
    })
}

/// The catalogue pattern of an edit when it is one of the fields that names
/// its entry, which a batch must apply after the edit's other changes.
fn identity_pattern<'a>(
    edit: &'a SettingsEditRequest,
    document: &'a SettingsDocument,
) -> Option<&'a str> {
    let pattern = if edit.pattern.is_empty() {
        find_key(document, &edit.key_path).map(|key| key.pattern.as_str())?
    } else {
        edit.pattern.as_str()
    };
    matches!(pattern, "orgs[].name" | "orgs[].projects[].name" | "accounts[].display_name")
        .then_some(pattern)
}

fn apply_one(
    doc: &mut DocumentMut,
    edit: &SettingsEditRequest,
    document: &SettingsDocument,
) -> Result<(), WriteError> {
    match edit.op {
        SettingsEditOp::Set => set_key(doc, edit, document),
        SettingsEditOp::Remove if edit.pattern.is_empty() => remove_entry(doc, edit),
        SettingsEditOp::Remove => remove_key(doc, edit, document),
        SettingsEditOp::Add => add_entry(doc, edit),
    }
}

fn find_key<'a>(document: &'a SettingsDocument, path: &str) -> Option<&'a SettingsKey> {
    document.areas.iter().flat_map(|area| &area.keys).find(|key| key.path == path)
}

fn set_key(
    doc: &mut DocumentMut,
    edit: &SettingsEditRequest,
    document: &SettingsDocument,
) -> Result<(), WriteError> {
    let key = find_key(document, &edit.key_path).ok_or_else(|| {
        invalid(
            format!("`{}` is no longer in this config", edit.key_path),
            Some(edit.key_path.clone()),
        )
    })?;
    if !edit.pattern.is_empty() && edit.pattern != key.pattern {
        return Err(invalid(
            format!("`{}` is not the key it was loaded as", edit.key_path),
            Some(edit.key_path.clone()),
        ));
    }
    let value = edit.value.clone().unwrap_or(serde_json::Value::Null);
    if value.is_null() {
        return remove_leaf(doc, &key.pattern, &edit.key_path);
    }
    let toml = to_toml(&value, key.edit.kind, &key.edit.allowed, &edit.key_path)?;
    let (steps, leaf) = locate(&key.pattern, &edit.key_path).ok_or_else(|| {
        invalid(
            format!("`{}` does not fill its catalogue pattern", edit.key_path),
            Some(edit.key_path.clone()),
        )
    })?;
    let parent = descend(doc, &steps, true).ok_or_else(|| {
        invalid(
            format!("`{}` has no table to write into", edit.key_path),
            Some(edit.key_path.clone()),
        )
    })?;
    parent.set(&leaf, toml);
    Ok(())
}

fn remove_key(
    doc: &mut DocumentMut,
    edit: &SettingsEditRequest,
    document: &SettingsDocument,
) -> Result<(), WriteError> {
    let key = find_key(document, &edit.key_path).ok_or_else(|| {
        invalid(
            format!("`{}` is no longer in this config", edit.key_path),
            Some(edit.key_path.clone()),
        )
    })?;
    remove_leaf(doc, &key.pattern, &edit.key_path)
}

fn remove_leaf(doc: &mut DocumentMut, pattern: &str, path: &str) -> Result<(), WriteError> {
    let (steps, leaf) = locate(pattern, path).ok_or_else(|| {
        invalid(format!("`{path}` does not fill its catalogue pattern"), Some(path.to_owned()))
    })?;
    let parent = descend(doc, &steps, false).ok_or_else(|| {
        invalid(format!("`{path}` has no table to remove from"), Some(path.to_owned()))
    })?;
    parent.remove(&leaf);
    Ok(())
}

/// One step into the document.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Key(String),
    Account(String),
    Org(String),
    Project(String),
    Slack(String),
}

/// Where a leaf sits: the steps to its parent and the leaf's own key.
fn locate(pattern: &str, path: &str) -> Option<(Vec<Step>, String)> {
    let parts = parts_of(pattern)?;
    let regex = regex_for(&parts)?;
    let captures = regex.captures(path)?;
    let wildcards: Vec<String> = captures
        .iter()
        .skip(1)
        .filter_map(|capture| capture.map(|c| c.as_str().to_owned()))
        .collect();
    let mut next = 0usize;
    let mut take = || {
        let value = wildcards.get(next).cloned();
        next += 1;
        value
    };
    let last = parts.last()?;
    let leaf = match last {
        // A trailing wildcard is the leaf, and it is the LAST capture: the
        // captures before it are the steps.
        Part::Wildcard => wildcards.last()?.clone(),
        Part::Literal(value) | Part::Array(value) => value.clone(),
    };
    let mut steps = Vec::new();
    for part in parts.iter().take(parts.len() - 1) {
        match part {
            Part::Literal(value) => steps.push(Step::Key(value.clone())),
            Part::Array(value) => match value.as_str() {
                "accounts" => steps.push(Step::Account(take()?)),
                "orgs" => steps.push(Step::Org(take()?)),
                "projects" => steps.push(Step::Project(take()?)),
                "slack" => steps.push(Step::Slack(take()?)),
                _ => return None,
            },
            Part::Wildcard => steps.push(Step::Key(take()?)),
        }
    }
    Some((steps, leaf))
}

#[derive(Clone, Debug)]
enum Part {
    Literal(String),
    /// `name[]`: the table's name and one wildcard for the instance.
    Array(String),
    Wildcard,
}

fn parts_of(pattern: &str) -> Option<Vec<Part>> {
    let mut parts = Vec::new();
    for segment in pattern.split('.') {
        if segment.is_empty() {
            return None;
        }
        if segment == "*" {
            parts.push(Part::Wildcard);
        } else if let Some(name) = segment.strip_suffix("[]") {
            parts.push(Part::Array(name.to_owned()));
        } else {
            parts.push(Part::Literal(segment.to_owned()));
        }
    }
    (!parts.is_empty()).then_some(parts)
}

fn regex_for(parts: &[Part]) -> Option<Regex> {
    let mut pattern = String::from("^");
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            pattern.push_str("\\.");
        }
        match part {
            Part::Literal(value) => pattern.push_str(&regex::escape(value)),
            Part::Array(value) => {
                pattern.push_str(&regex::escape(value));
                pattern.push_str("\\.");
                let wildcard = if index + 1 == parts.len() { "(.+)" } else { "(.+?)" };
                pattern.push_str(wildcard);
            }
            Part::Wildcard => {
                let wildcard = if index + 1 == parts.len() { "(.+)" } else { "(.+?)" };
                pattern.push_str(wildcard);
            }
        }
    }
    pattern.push('$');
    Regex::new(&pattern).ok()
}

/// The container a leaf's parent is: a table, or an inline table
/// (`model_slugs = { ... }`).
enum Container<'a> {
    Table(&'a mut Table),
    Inline(&'a mut InlineTable),
}

impl Container<'_> {
    fn set(self, key: &str, value: Value) {
        match self {
            Self::Table(table) => match table.get_mut(key) {
                Some(item) => set_item_value(item, value),
                None => {
                    table.insert(key, Item::Value(value));
                }
            },
            Self::Inline(table) => match table.get_mut(key) {
                Some(existing) => {
                    let decor = existing.decor().clone();
                    let mut value = value;
                    *value.decor_mut() = decor;
                    *existing = value;
                }
                None => {
                    table.insert(key, value);
                }
            },
        }
    }

    fn remove(self, key: &str) {
        match self {
            Self::Table(table) => {
                // **A key's comments live in its decor prefix**, so removing
                // the key must move them rather than delete them: onto the
                // key that follows it, or the table's own trailing text when
                // it was the last one.
                let next = next_key_after(table, key);
                // Read the leading text before the removal: a comment above
                // the key lives on the key's own leaf decor, and a trailing
                // one on the value's suffix.
                let prefix = table.get_key_value(key).map(|(held, item)| {
                    let mut text = key_text(held);
                    let suffix = item_suffix(item);
                    if !suffix.trim().is_empty() {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(suffix.trim_start());
                        if !text.ends_with('\n') {
                            text.push('\n');
                        }
                    }
                    text
                });
                let removed = table.remove(key);
                let Some(prefix) = prefix else { return };
                if removed.is_none() || prefix.trim().is_empty() {
                    return;
                }
                if let Some((mut next, _)) = next.and_then(|next| table.get_key_value_mut(&next)) {
                    let existing = next
                        .leaf_decor()
                        .prefix()
                        .and_then(|raw| raw.as_str())
                        .map(str::to_owned)
                        .unwrap_or_default();
                    next.leaf_decor_mut().set_prefix(format!("{prefix}{existing}"));
                } else {
                    let existing = table
                        .decor()
                        .suffix()
                        .and_then(|raw| raw.as_str())
                        .map(str::to_owned)
                        .unwrap_or_default();
                    table.decor_mut().set_suffix(format!("{existing}{prefix}"));
                }
            }
            Self::Inline(table) => {
                table.remove(key);
            }
        }
    }
}

/// The key after `key` in the table's own order, before it is removed.
fn next_key_after(table: &Table, key: &str) -> Option<String> {
    let mut seen = false;
    for (candidate, _) in table {
        if seen {
            return Some(candidate.to_owned());
        }
        if candidate == key {
            seen = true;
        }
    }
    None
}

/// The comments and whitespace a table entry's key carries before it.
fn key_text(key: &toml_edit::Key) -> String {
    key.leaf_decor().prefix().and_then(|raw| raw.as_str()).map(str::to_owned).unwrap_or_default()
}

/// The trailing comment a value carries on its own line.
fn item_suffix(item: &Item) -> String {
    match item {
        Item::Value(value) => value
            .decor()
            .suffix()
            .and_then(|raw| raw.as_str())
            .map(str::to_owned)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Remove one table from an array of tables, moving its comments onto the
/// entry that follows (or the previous entry's trailing text when it was
/// last) so a removal does not take the reader's notes with it.
fn remove_entry_at(array: &mut ArrayOfTables, index: usize) {
    let prefix = array
        .get(index)
        .and_then(|table| table.decor().prefix().and_then(|raw| raw.as_str()).map(str::to_owned));
    let Some(prefix) = prefix.filter(|text| !text.trim().is_empty()) else {
        array.remove(index);
        return;
    };
    array.remove(index);
    if let Some(next) = array.get_mut(index) {
        let existing = next
            .decor()
            .prefix()
            .and_then(|raw| raw.as_str())
            .map(str::to_owned)
            .unwrap_or_default();
        next.decor_mut().set_prefix(format!("{prefix}{existing}"));
    } else if index > 0
        && let Some(previous) = array.get_mut(index - 1)
    {
        let existing = previous
            .decor()
            .suffix()
            .and_then(|raw| raw.as_str())
            .map(str::to_owned)
            .unwrap_or_default();
        previous.decor_mut().set_suffix(format!("{existing}{prefix}"));
    }
}

fn set_item_value(item: &mut Item, value: Value) {
    match item {
        Item::Value(existing) => {
            let decor = existing.decor().clone();
            let mut value = value;
            *value.decor_mut() = decor;
            *existing = value;
        }
        _ => *item = Item::Value(value),
    }
}

fn descend<'a>(doc: &'a mut DocumentMut, steps: &[Step], create: bool) -> Option<Container<'a>> {
    descend_from(Container::Table(doc.as_table_mut()), steps, create)
}

fn descend_from<'a>(
    container: Container<'a>,
    steps: &[Step],
    create: bool,
) -> Option<Container<'a>> {
    let Some((step, rest)) = steps.split_first() else { return Some(container) };
    let next: Container<'a> = match step {
        Step::Key(key) => match container {
            Container::Table(table) => {
                if create && table.get(key).is_none() {
                    table.insert(key, Item::Table(Table::new()));
                }
                match table.get_mut(key)? {
                    Item::Table(table) => Container::Table(table),
                    Item::Value(Value::InlineTable(table)) => Container::Inline(table),
                    _ => return None,
                }
            }
            Container::Inline(_) => return None,
        },
        Step::Account(name) => {
            let Container::Table(root) = container else { return None };
            let array = root.get_mut("accounts")?.as_array_of_tables_mut()?;
            let index = array
                .iter()
                .position(|table| table.get("display_name").and_then(Item::as_str) == Some(name))?;
            Container::Table(array.get_mut(index)?)
        }
        Step::Org(name) => {
            let Container::Table(root) = container else { return None };
            let array = root.get_mut("orgs")?.as_array_of_tables_mut()?;
            let index = array
                .iter()
                .position(|table| table.get("name").and_then(Item::as_str) == Some(name))?;
            Container::Table(array.get_mut(index)?)
        }
        Step::Slack(name) => {
            let Container::Table(root) = container else { return None };
            let array = root.get_mut("slack")?.as_array_of_tables_mut()?;
            let index = array
                .iter()
                .position(|table| table.get("workspace").and_then(Item::as_str) == Some(name))?;
            Container::Table(array.get_mut(index)?)
        }
        Step::Project(name) => {
            let Container::Table(org) = container else { return None };
            let array = org.get_mut("projects")?.as_array_of_tables_mut()?;
            let index = array
                .iter()
                .position(|table| table.get("name").and_then(Item::as_str) == Some(name))?;
            Container::Table(array.get_mut(index)?)
        }
    };
    descend_from(next, rest, create)
}

/// Convert a JSON value to the TOML shape its edit kind names.
fn to_toml(
    value: &serde_json::Value,
    kind: EditKind,
    allowed: &[String],
    path: &str,
) -> Result<Value, WriteError> {
    let bad = |what: &str| invalid(format!("`{path}` wants {what}"), Some(path.to_owned()));
    match kind {
        EditKind::String => {
            let value = value.as_str().ok_or_else(|| bad("a string"))?;
            Ok(Value::from(value.to_owned()))
        }
        EditKind::Enum => {
            let value = value.as_str().ok_or_else(|| bad("one of its names"))?;
            if !allowed.iter().any(|name| name == value) {
                return Err(invalid(
                    format!("`{path}` accepts {}; `{value}` is not one", allowed.join(", ")),
                    Some(path.to_owned()),
                ));
            }
            Ok(Value::from(value.to_owned()))
        }
        EditKind::Number => {
            if let Some(number) = value.as_i64() {
                Ok(Value::from(number))
            } else if let Some(number) = value.as_u64() {
                i64::try_from(number).map(Value::from).map_err(|_| bad("a whole number"))
            } else {
                Err(bad("a whole number"))
            }
        }
        EditKind::Bool => {
            let value = value.as_bool().ok_or_else(|| bad("true or false"))?;
            Ok(Value::from(value))
        }
        EditKind::List => {
            let items = value.as_array().ok_or_else(|| bad("a list"))?;
            let mut array = Array::new();
            for item in items {
                let text = item.as_str().ok_or_else(|| bad("a list of strings"))?;
                array.push(Value::from(text.to_owned()));
            }
            Ok(Value::Array(array))
        }
    }
}

/// Apply an entry's fields to a fresh table.
fn entry_table(
    value: &serde_json::Value,
    name_field: Option<&str>,
    name: &str,
) -> Result<Table, WriteError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("an entry is added with its fields", Some(name.to_owned())))?;
    let mut table = Table::new();
    for (key, held) in object {
        let Some(item) = json_item(held, key)? else { continue };
        table.insert(key, item);
    }
    if let Some(field) = name_field
        && table.get(field).is_none()
    {
        table.insert(field, Item::Value(Value::from(name.to_owned())));
    }
    Ok(table)
}

fn json_item(value: &serde_json::Value, path: &str) -> Result<Option<Item>, WriteError> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(value) => Ok(Some(Item::Value(Value::from(*value)))),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(Some(Item::Value(Value::from(value))))
            } else if let Some(value) = number.as_u64() {
                i64::try_from(value).map(|value| Some(Item::Value(Value::from(value)))).map_err(
                    |_| invalid(format!("`{path}` wants a whole number"), Some(path.to_owned())),
                )
            } else {
                Err(invalid(format!("`{path}` wants a whole number"), Some(path.to_owned())))
            }
        }
        serde_json::Value::String(value) => Ok(Some(Item::Value(Value::from(value.clone())))),
        serde_json::Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                let text = item.as_str().ok_or_else(|| {
                    invalid(format!("`{path}` wants a list of strings"), Some(path.to_owned()))
                })?;
                array.push(Value::from(text.to_owned()));
            }
            Ok(Some(Item::Value(Value::Array(array))))
        }
        serde_json::Value::Object(fields) => {
            let mut table = Table::new();
            for (key, held) in fields {
                if let Some(item) = json_item(held, key)? {
                    table.insert(key, item);
                }
            }
            Ok(Some(Item::Table(table)))
        }
    }
}

/// Add a whole entry: an account, a project under an org, a Slack workspace,
/// or a model table.
fn add_entry(doc: &mut DocumentMut, edit: &SettingsEditRequest) -> Result<(), WriteError> {
    let value = edit
        .value
        .as_ref()
        .ok_or_else(|| invalid("an entry is added with its fields", Some(edit.key_path.clone())))?;
    match edit.area.as_str() {
        "accounts" => {
            let name = strip_prefix(&edit.key_path, "accounts.").ok_or_else(|| {
                invalid("an account's path names the account", Some(edit.key_path.clone()))
            })?;
            let table = entry_table(value, Some("display_name"), name)?;
            let exists =
                doc.as_table().get("accounts").and_then(Item::as_array_of_tables).is_some_and(
                    |array| {
                        array.iter().any(|held| {
                            held.get("display_name").and_then(Item::as_str) == Some(name)
                        })
                    },
                );
            if exists {
                return Err(invalid(
                    format!("an account named `{name}` already exists"),
                    Some(edit.key_path.clone()),
                ));
            }
            let accounts = doc
                .as_table_mut()
                .entry("accounts")
                .or_insert(Item::ArrayOfTables(ArrayOfTables::default()));
            let array = accounts.as_array_of_tables_mut().ok_or_else(|| {
                invalid("`accounts` is not an array of tables", Some(edit.key_path.clone()))
            })?;
            array.push(table);
            Ok(())
        }
        "connectors" => {
            let name = strip_prefix(&edit.key_path, "slack.").ok_or_else(|| {
                invalid("a Slack entry's path names the workspace", Some(edit.key_path.clone()))
            })?;
            let table = entry_table(value, Some("workspace"), name)?;
            let exists =
                doc.as_table().get("slack").and_then(Item::as_array_of_tables).is_some_and(
                    |array| {
                        array
                            .iter()
                            .any(|held| held.get("workspace").and_then(Item::as_str) == Some(name))
                    },
                );
            if exists {
                return Err(invalid(
                    format!("a Slack workspace named `{name}` already exists"),
                    Some(edit.key_path.clone()),
                ));
            }
            let slack = doc
                .as_table_mut()
                .entry("slack")
                .or_insert(Item::ArrayOfTables(ArrayOfTables::default()));
            let array = slack.as_array_of_tables_mut().ok_or_else(|| {
                invalid("`slack` is not an array of tables", Some(edit.key_path.clone()))
            })?;
            array.push(table);
            Ok(())
        }
        "models" => {
            let name = strip_prefix(&edit.key_path, "models.").ok_or_else(|| {
                invalid("a model's path names the model", Some(edit.key_path.clone()))
            })?;
            let table = entry_table(value, None, name)?;
            let exists = doc
                .as_table()
                .get("models")
                .and_then(Item::as_table)
                .is_some_and(|models| models.contains_key(name));
            if exists {
                return Err(invalid(
                    format!("a model named `{name}` already exists"),
                    Some(edit.key_path.clone()),
                ));
            }
            let models = doc.as_table_mut().entry("models").or_insert(Item::Table(Table::new()));
            let models = models
                .as_table_mut()
                .ok_or_else(|| invalid("`models` is not a table", Some(edit.key_path.clone())))?;
            models.insert(name, Item::Table(table));
            Ok(())
        }
        "projects" => {
            let Some((org, project)) = project_path(doc, &edit.key_path) else {
                return Err(invalid(
                    "a project's path names its org and the project",
                    Some(edit.key_path.clone()),
                ));
            };
            let table = entry_table(value, Some("name"), &project)?;
            let orgs = doc
                .as_table_mut()
                .get_mut("orgs")
                .ok_or_else(|| invalid("this config has no orgs", Some(edit.key_path.clone())))?;
            let Item::ArrayOfTables(orgs) = orgs else {
                return Err(invalid(
                    "`orgs` is not an array of tables",
                    Some(edit.key_path.clone()),
                ));
            };
            let index = orgs
                .iter()
                .position(|table| table.get("name").and_then(Item::as_str) == Some(org.as_str()))
                .ok_or_else(|| {
                    invalid(format!("no org named `{org}`"), Some(edit.key_path.clone()))
                })?;
            let org_table = orgs
                .get_mut(index)
                .ok_or_else(|| invalid("the org went away", Some(edit.key_path.clone())))?;
            let projects = org_table
                .entry("projects")
                .or_insert(Item::ArrayOfTables(ArrayOfTables::default()));
            let array = projects.as_array_of_tables_mut().ok_or_else(|| {
                invalid("`projects` is not an array of tables", Some(edit.key_path.clone()))
            })?;
            let exists = array
                .iter()
                .any(|held| held.get("name").and_then(Item::as_str) == Some(project.as_str()));
            if exists {
                return Err(invalid(
                    format!("a project named `{project}` already exists under `{org}`"),
                    Some(edit.key_path.clone()),
                ));
            }
            array.push(table);
            Ok(())
        }
        area => {
            Err(invalid(format!("`{area}` does not add entries here"), Some(edit.key_path.clone())))
        }
    }
}

/// Remove a whole entry.
fn remove_entry(doc: &mut DocumentMut, edit: &SettingsEditRequest) -> Result<(), WriteError> {
    let not_found = || invalid("that entry is not in this file", Some(edit.key_path.clone()));
    match edit.area.as_str() {
        "accounts" => {
            let name = strip_prefix(&edit.key_path, "accounts.").ok_or_else(not_found)?;
            let accounts = doc
                .as_table_mut()
                .get_mut("accounts")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(not_found)?;
            let index = accounts
                .iter()
                .position(|table| table.get("display_name").and_then(Item::as_str) == Some(name))
                .ok_or_else(not_found)?;
            remove_entry_at(accounts, index);
            Ok(())
        }
        "connectors" => {
            let name = strip_prefix(&edit.key_path, "slack.").ok_or_else(not_found)?;
            let slack = doc
                .as_table_mut()
                .get_mut("slack")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(not_found)?;
            let index = slack
                .iter()
                .position(|table| table.get("workspace").and_then(Item::as_str) == Some(name))
                .ok_or_else(not_found)?;
            remove_entry_at(slack, index);
            Ok(())
        }
        "models" => {
            let name = strip_prefix(&edit.key_path, "models.").ok_or_else(not_found)?;
            let models = doc
                .as_table_mut()
                .get_mut("models")
                .and_then(Item::as_table_mut)
                .ok_or_else(not_found)?;
            models.remove(name);
            Ok(())
        }
        "projects" => {
            let (org, project) = project_path(doc, &edit.key_path).ok_or_else(not_found)?;
            let orgs = doc
                .as_table_mut()
                .get_mut("orgs")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(not_found)?;
            let org_index = orgs
                .iter()
                .position(|table| table.get("name").and_then(Item::as_str) == Some(org.as_str()))
                .ok_or_else(not_found)?;
            let org_table = orgs.get_mut(org_index).ok_or_else(not_found)?;
            let projects = org_table
                .get_mut("projects")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(not_found)?;
            let index = projects
                .iter()
                .position(|table| {
                    table.get("name").and_then(Item::as_str) == Some(project.as_str())
                })
                .ok_or_else(not_found)?;
            remove_entry_at(projects, index);
            Ok(())
        }
        _ => Err(not_found()),
    }
}

fn strip_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)
}

/// The org and project a project entry path names, matched against the
/// document's own org names: an org name containing dots (or the literal
/// `.projects.`) cannot be cut short by a split.
fn project_path(doc: &DocumentMut, path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("orgs.")?;
    let orgs = doc.as_table().get("orgs")?.as_array_of_tables()?;
    let mut best: Option<(String, String)> = None;
    for table in orgs {
        let Some(org) = table.get("name").and_then(Item::as_str) else { continue };
        let Some(tail) = rest.strip_prefix(org).and_then(|tail| tail.strip_prefix(".projects."))
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(held, _)| org.len() > held.len()) {
            best = Some((org.to_owned(), tail.to_owned()));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use forge_primitives::settings::{SettingsDocument, SettingsEditOp, SettingsEditRequest};

    use super::*;

    /// A config with comments, two accounts, an org and its projects, and a
    /// model table, so an edit can be read against the text around it.
    const FIXTURE: &str = r#"# forge.toml - the fixture
[server]
# the socket's port
port = 8790  # a hand-written comment
bind = "127.0.0.1"

[models."deepseek-v4.1-flash"]
auto_compact_window = 983040

# the first account
[[accounts]]
display_name = "One"
provider = "anthropic"
token = "one-token"
models = ["deepseek-v4.1-flash"]
model_slugs = { "deepseek-v4.1-flash" = "deepseek/deepseek-v4.1-flash" }

[accounts.env]
FLAG = "1"

# the second account
[[accounts]]
display_name = "Two"
provider = "anthropic"
token = "two-token"
models = ["deepseek-v4.1-flash"]

[[orgs]]
name = "Acme.Corp"
accounts = ["One"]

[[orgs.projects]]
name = "api.v2"
path = "/tmp/api.v2"
model = "deepseek-v4.1-flash"
permission_mode = "auto"

[orgs.projects.env]
SECRET = "s"

[[orgs.projects]]
name = "site"
path = "/tmp/site"
model = "deepseek-v4.1-flash"

[[slack]]
workspace = "Work"
token = "xoxp-example"
"#;

    fn document(raw: &str) -> SettingsDocument {
        let config =
            crate::config::load_from_text(std::path::PathBuf::from("/tmp/forge.toml"), raw)
                .expect("the fixture loads");
        crate::settings::document(&config, "v", Path::new("/tmp/forge.toml"))
    }

    fn key(document: &SettingsDocument, path: &str) -> (String, String) {
        let found = document
            .areas
            .iter()
            .flat_map(|area| &area.keys)
            .find(|key| key.path == path)
            .unwrap_or_else(|| panic!("{path} has no row"));
        (found.pattern.clone(), found.path.clone())
    }

    fn set_edit(
        document: &SettingsDocument,
        path: &str,
        value: serde_json::Value,
    ) -> SettingsEditRequest {
        let (pattern, key_path) = key(document, path);
        let area = document
            .areas
            .iter()
            .find(|area| area.keys.iter().any(|key| key.path == path))
            .map(|area| area.id.clone())
            .expect("the key has an area");
        SettingsEditRequest { area, key_path, pattern, op: SettingsEditOp::Set, value: Some(value) }
    }

    fn entry_edit(
        area: &str,
        key_path: &str,
        op: SettingsEditOp,
        value: Option<serde_json::Value>,
    ) -> SettingsEditRequest {
        SettingsEditRequest {
            area: area.to_owned(),
            key_path: key_path.to_owned(),
            pattern: String::new(),
            op,
            value,
        }
    }

    /// The key's value is replaced, and every comment and every line that
    /// did not change stays exactly where it was.
    #[test]
    fn a_set_preserves_the_comments_and_the_order() {
        let raw = FIXTURE;
        let document = document(raw);
        let edit = set_edit(&document, "server.port", serde_json::json!(8791));
        let out = apply(raw, &[edit], &document).expect("the edit applies");
        assert!(out.contains("# forge.toml - the fixture"), "the top comment stayed: {out}");
        assert!(out.contains("# the socket's port"), "the key's comment stayed: {out}");
        assert!(out.contains("# a hand-written comment"), "the inline comment stayed: {out}");
        assert!(out.contains("port = 8791"), "the value moved: {out}");
        assert!(
            out.find("[server]") < out.find("[[accounts]]"),
            "the tables kept their order: {out}",
        );
    }

    /// A value of the wrong shape is refused before anything else is tried.
    #[test]
    fn a_string_is_refused_for_a_number() {
        let document = document(FIXTURE);
        let edit = set_edit(&document, "server.port", serde_json::json!("not a number"));
        let error = apply(FIXTURE, &[edit], &document).expect_err("a string is not a port");
        assert!(error.message.contains("want"), "the message names the shape: {error:?}");
        assert_eq!(error.key_path.as_deref(), Some("server.port"));
    }

    /// A project whose org and name carry dots is found by its pattern, not
    /// by splitting the display path on every dot.
    #[test]
    fn a_dotted_project_name_is_located_by_its_pattern() {
        let document = document(FIXTURE);
        let edit = set_edit(
            &document,
            "orgs.Acme.Corp.projects.api.v2.model",
            serde_json::json!("deepseek-v4.1-flash"),
        );
        let out = apply(FIXTURE, &[edit], &document).expect("the dotted project applies");
        assert!(out.contains("[[orgs.projects]]\nname = \"api.v2\""), "{out}");
        assert!(out.contains("model = \"deepseek-v4.1-flash\""), "{out}");
    }

    /// An inline table's entry is rewritten in place.
    #[test]
    fn an_inline_table_entry_is_set() {
        let document = document(FIXTURE);
        let edit = set_edit(
            &document,
            "accounts.One.model_slugs.deepseek-v4.1-flash",
            serde_json::json!("other/flash"),
        );
        let out = apply(FIXTURE, &[edit], &document).expect("the slug applies");
        assert!(out.contains("\"other/flash\""), "the slug moved: {out}");
        assert!(out.contains("display_name = \"One\""), "the account stayed: {out}");
    }

    /// An optional key removed from the file is gone, and its neighbours are
    /// untouched.
    #[test]
    fn a_leaf_removal_takes_only_that_key() {
        let document = document(FIXTURE);
        let (pattern, key_path) = key(&document, "accounts.One.env.FLAG");
        let edit = SettingsEditRequest {
            area: "accounts".to_owned(),
            key_path,
            pattern,
            op: SettingsEditOp::Remove,
            value: None,
        };
        let out = apply(FIXTURE, &[edit], &document).expect("the removal applies");
        assert!(!out.contains("FLAG = \"1\""), "the key went: {out}");
        assert!(out.contains("[accounts.env]"), "the empty table stays: {out}");
    }

    /// A new project is added under its org with the fields the page sent.
    #[test]
    fn a_project_is_added_under_its_org() {
        let document = document(FIXTURE);
        let edit = entry_edit(
            "projects",
            "orgs.Acme.Corp.projects.new-one",
            SettingsEditOp::Add,
            Some(serde_json::json!({
                "name": "new-one",
                "path": "/tmp/new-one",
                "model": "deepseek-v4.1-flash",
            })),
        );
        let out = apply(FIXTURE, &[edit], &document).expect("the project adds");
        assert!(out.contains("name = \"new-one\""), "the project is in the text: {out}");
        assert!(out.contains("path = \"/tmp/new-one\""), "{out}");
    }

    /// An account is removed whole.
    #[test]
    fn an_account_is_removed_whole() {
        let document = document(FIXTURE);
        let edit = entry_edit("accounts", "accounts.Two", SettingsEditOp::Remove, None);
        let out = apply(FIXTURE, &[edit], &document).expect("the account removes");
        assert!(!out.contains("\"Two\""), "the account went: {out}");
        assert!(out.contains("display_name = \"One\""), "the other stayed: {out}");
    }

    /// A model table is added and removed by name.
    #[test]
    fn a_model_table_is_added_and_removed() {
        let document = document(FIXTURE);
        let add = entry_edit(
            "models",
            "models.new-model",
            SettingsEditOp::Add,
            Some(serde_json::json!({ "auto_compact_window": 1024 })),
        );
        let out = apply(FIXTURE, &[add], &document).expect("the model adds");
        assert!(out.contains("[models.new-model]"), "{out}");
        let remove = entry_edit("models", "models.new-model", SettingsEditOp::Remove, None);
        let out = apply(&out, &[remove], &document).expect("the model removes");
        assert!(!out.contains("[models.new-model]"), "{out}");
        assert!(out.contains("[models.\"deepseek-v4.1-flash\"]"), "the other stayed: {out}");
    }

    /// A Slack workspace's pump token changes in place.
    #[test]
    fn a_slack_token_is_set() {
        let document = document(FIXTURE);
        let edit = set_edit(&document, "slack.Work.token", serde_json::json!("xoxp-new"));
        let out = apply(FIXTURE, &[edit], &document).expect("the token applies");
        assert!(out.contains("xoxp-new"), "{out}");
        assert!(out.contains("workspace = \"Work\""), "{out}");
    }

    /// **A removal keeps the comments above the key.** The port carries a
    /// hand-written note; removing the key must move the note, not delete it.
    #[test]
    fn a_key_removal_keeps_its_comments() {
        let document = document(FIXTURE);
        let (pattern, key_path) = key(&document, "server.port");
        let edit = SettingsEditRequest {
            area: "server".to_owned(),
            key_path,
            pattern,
            op: SettingsEditOp::Remove,
            value: None,
        };
        let out = apply(FIXTURE, &[edit], &document).expect("the key removes");
        assert!(!out.contains("port = 8790"), "the key is gone: {out}");
        assert!(out.contains("# the socket's port"), "the note above it stayed: {out}");
    }

    /// **A removal keeps the comments above the entry** it takes.
    #[test]
    fn an_entry_removal_keeps_its_comments() {
        let document = document(FIXTURE);
        let edit = entry_edit("accounts", "accounts.One", SettingsEditOp::Remove, None);
        let out = apply(FIXTURE, &[edit], &document).expect("the account removes");
        assert!(!out.contains("display_name = \"One\""), "the entry is gone: {out}");
        assert!(out.contains("# the first account"), "its note stayed: {out}");
    }

    /// **A CRLF config keeps its line endings.** toml_edit renders `\n`; the
    /// writer must hand back the file's own style.
    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        let raw = FIXTURE.replace('\n', "\r\n");
        let document = document(&raw);
        let edit = set_edit(&document, "server.port", serde_json::json!(8791));
        let out = apply(&raw, &[edit], &document).expect("the edit applies");
        assert!(out.contains("\r\n"), "still CRLF: {out:?}");
        assert!(!out.replace("\r\n", "").contains('\n'), "no bare LF was introduced: {out:?}");
    }

    /// **A rename and an edit of the same entry share one batch.** The edit
    /// is located by the pre-edit name, so the rename must be applied after
    /// it whatever order the batch arrived in.
    #[test]
    fn a_rename_and_an_edit_of_the_same_entry_apply_together() {
        let document = document(FIXTURE);
        let rename =
            set_edit(&document, "orgs.Acme.Corp.projects.api.v2.name", serde_json::json!("api.v3"));
        let edit = set_edit(
            &document,
            "orgs.Acme.Corp.projects.api.v2.auto_start",
            serde_json::json!(true),
        );
        let out = apply(FIXTURE, &[rename, edit], &document).expect("both apply");
        assert!(out.contains("name = \"api.v3\""), "the rename landed: {out}");
        assert!(out.contains("auto_start = true"), "the edit landed: {out}");
    }

    /// **An add cannot replace an existing entry.** A model table already in
    /// the file is refused rather than silently overwritten.
    #[test]
    fn adding_an_existing_model_is_refused() {
        let document = document(FIXTURE);
        let edit = entry_edit(
            "models",
            "models.deepseek-v4.1-flash",
            SettingsEditOp::Add,
            Some(serde_json::json!({ "auto_compact_window": 1 })),
        );
        let error = apply(FIXTURE, &[edit], &document).expect_err("the add is refused");
        assert!(error.message.contains("already exists"), "{}", error.message);
    }

    /// **A dotted org is matched from the document's own names.** An org
    /// whose name contains `.projects.` cannot be cut short by a split.
    #[test]
    fn a_project_under_a_dotted_org_is_removed_by_its_own_org() {
        let raw = FIXTURE.replace("name = \"Acme.Corp\"", "name = \"Acme.projects.Team\"");
        let document = document(&raw);
        let edit = entry_edit(
            "projects",
            "orgs.Acme.projects.Team.projects.api.v2",
            SettingsEditOp::Remove,
            None,
        );
        let out = apply(&raw, &[edit], &document).expect("the right project removes");
        assert!(!out.contains("name = \"api.v2\""), "the project is gone: {out}");
        assert!(out.contains("name = \"site\""), "its sibling stayed: {out}");
    }
}
