//! Durable project policy: the notes a teaching answer writes.
//!
//! One note per `(project, class)`: a second lesson on the same class
//! replaces the first, because the note is the class's standing rule rather
//! than a log. The project's leads read the set at spawn and through
//! `tasks__policy`, and the drive reads it so an ask of a settled class is
//! not raised again.

use std::time::SystemTime;

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};

use super::Db;

const POLICY: TableDefinition<&str, &[u8]> = TableDefinition::new("project_policy");

/// One settled class of ask: the lesson the person gave, and the row it
/// was given on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicyNote {
    pub id: String,
    pub project_name: String,
    /// The recurring class of ask this note settles.
    pub class: String,
    pub text: String,
    /// The teaching row whose answer wrote it.
    pub source_task: String,
    pub at: SystemTime,
}

fn row_key(note: &PolicyNote) -> String {
    format!("{}\u{0}{}", note.project_name, note.class)
}

/// Write `note`, replacing any note the same class already had.
pub fn append(db: &Db, note: &PolicyNote) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(POLICY)?;
        let value = serde_json::to_vec(note).context("serialize policy note")?;
        table.insert(row_key(note).as_str(), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// `project`'s notes, by class.
pub fn list_for_project(db: &Db, project: &str) -> anyhow::Result<Vec<PolicyNote>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(POLICY) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let prefix = format!("{project}\u{0}");
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            continue;
        }
        match serde_json::from_slice::<PolicyNote>(value.value()) {
            Ok(note) => out.push(note),
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::policy",
                row = %key.value(),
                error = %err,
                "skipping a policy note that failed to decode",
            ),
        }
    }
    out.sort_by(|a, b| a.class.cmp(&b.class));
    Ok(out)
}

/// The note `project` holds for `class`, when it has settled one.
pub fn get_for_class(db: &Db, project: &str, class: &str) -> anyhow::Result<Option<PolicyNote>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(POLICY) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let key = format!("{project}\u{0}{class}");
    let Some(value) = table.get(key.as_str())? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice::<PolicyNote>(value.value()).ok())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn db(dir: &Path) -> Db {
        Db::open(&dir.join("db.redb")).expect("open")
    }

    fn note(class: &str, text: &str) -> PolicyNote {
        PolicyNote {
            id: format!("n-{class}"),
            project_name: "proj".to_owned(),
            class: class.to_owned(),
            text: text.to_owned(),
            source_task: "t-1".to_owned(),
            at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_note_is_read_by_class_and_the_latest_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = db(dir.path());
        append(&db, &note("ci-flake", "report it, never retry twice")).expect("append");
        append(&db, &note("ci-flake", "retry once, then report")).expect("replace");
        append(&db, &note("late-pr", "ask, do not merge")).expect("append");
        let notes = list_for_project(&db, "proj").expect("list");
        assert_eq!(notes.len(), 2, "one note per class");
        let flake = get_for_class(&db, "proj", "ci-flake").expect("get").expect("present");
        assert_eq!(flake.text, "retry once, then report", "the later lesson replaced the first");
        assert!(
            get_for_class(&db, "other", "ci-flake").expect("get").is_none(),
            "scoped by project"
        );
    }
}
