//! Biometric enrolments on the redb `biometric_enrolments` table.
//!
//! One row per device, keyed by the enrolment id its client minted. The whole
//! `Enrolment` is serde-json, blobs included: the bytes are the client's
//! wrapped secrets, stored and returned unchanged and never decoded here.

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};
use uuid::Uuid;

use super::Db;
use crate::biometric::Enrolment;

const ENROLMENTS: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("biometric_enrolments");

/// Write one device's enrolment, replacing any prior row with the same id.
pub(crate) fn put(db: &Db, enrolment: &Enrolment) -> anyhow::Result<()> {
    let value = serde_json::to_vec(enrolment).context("serialize biometric enrolment")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(ENROLMENTS)?;
        table.insert(enrolment.id.as_bytes(), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// The enrolment `id` names, or `None` when no row carries it.
///
/// A row that does not decode is an ERROR rather than `None`: a release that
/// read it as "nothing enrolled" would send the reader to the password path
/// while the enrolment sits right there, unreadable.
pub(crate) fn get(db: &Db, id: Uuid) -> anyhow::Result<Option<Enrolment>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(ENROLMENTS) {
        Ok(table) => table,
        // A fresh database has no table until the first write; an absent
        // table holds no enrolment.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(value) = table.get(id.as_bytes())? else { return Ok(None) };
    let enrolment = serde_json::from_slice(value.value())
        .with_context(|| format!("decode biometric enrolment {id}"))?;
    Ok(Some(enrolment))
}

/// Every stored enrolment.
///
/// One undecodable record is skipped with a warn rather than wiping the set:
/// a corrupt row must not hide the devices that are still readable. The
/// caller's own reads distinguish an empty set from an unreadable one.
pub(crate) fn list(db: &Db) -> anyhow::Result<Vec<Enrolment>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(ENROLMENTS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (id, value) = entry?;
        match serde_json::from_slice::<Enrolment>(value.value()) {
            Ok(enrolment) => out.push(enrolment),
            Err(error) => tracing::warn!(
                target: "forge_workspace::store::biometric",
                id = %Uuid::from_bytes(id.value()),
                %error,
                "skipping biometric enrolment record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Remove the enrolment `id` names. Returns whether a row was there.
pub(crate) fn remove(db: &Db, id: Uuid) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = match txn.open_table(ENROLMENTS) {
        Ok(mut table) => table.remove(id.as_bytes())?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(error) => return Err(error.into()),
    };
    txn.commit()?;
    Ok(existed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biometric::StoredPurpose;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn enrolment(id: Uuid, purposes: &[(&str, &[u8])]) -> Enrolment {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        Enrolment {
            id,
            device: "Ved's MacBook Pro".to_owned(),
            created_at: at,
            purposes: purposes
                .iter()
                .map(|(purpose, blob)| StoredPurpose {
                    purpose: (*purpose).to_owned(),
                    blob: blob.to_vec(),
                    created_at: at,
                    last_used_at: None,
                })
                .collect(),
        }
    }

    #[test]
    fn an_enrolment_round_trips_through_the_table() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let id = Uuid::new_v4();
        let written = enrolment(id, &[("vault", b"wrapped")]);

        put(&db, &written).expect("put");
        let read = get(&db, id).expect("get").expect("the row");
        assert_eq!(read, written, "the stored row is the row that was written");
        assert_eq!(list(&db).expect("list"), vec![written]);
    }

    /// A second purpose is a second entry in the same row with its own blob
    /// shape; the table has no per-purpose schema to change.
    #[test]
    fn one_row_carries_every_purpose_with_its_own_shape() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let id = Uuid::new_v4();
        let second = [9u8; 512];

        put(&db, &enrolment(id, &[("vault", b"{\"token\":\"t\"}"), ("sso", &second)]))
            .expect("put");

        let read = get(&db, id).expect("get").expect("the row");
        assert_eq!(read.purposes[0].blob, b"{\"token\":\"t\"}");
        assert_eq!(read.purposes[1].blob, second);
    }

    #[test]
    fn an_enrolment_survives_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        let id = Uuid::new_v4();
        {
            let db = Db::open(&path).expect("open db");
            put(&db, &enrolment(id, &[("vault", b"kept")])).expect("put");
        }
        let db = Db::open(&path).expect("reopen db");
        let read = get(&db, id).expect("get").expect("the row");
        assert_eq!(read.purposes[0].blob, b"kept");
    }

    /// A row that does not decode is skipped by the list and is an explicit
    /// error for the one read that named it, never a silent `None`.
    #[test]
    fn a_corrupt_row_is_skipped_by_the_list_and_refused_by_get() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let good = Uuid::new_v4();
        put(&db, &enrolment(good, &[("vault", b"good")])).expect("put the good row");

        let corrupt = Uuid::new_v4();
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(ENROLMENTS).expect("table");
            table.insert(corrupt.as_bytes(), "not an enrolment".as_bytes()).expect("insert");
        }
        txn.commit().expect("commit");

        assert_eq!(list(&db).expect("list tolerates the corrupt row").len(), 1);
        assert!(get(&db, corrupt).is_err(), "a named corrupt row is an error, not an absence");
    }

    #[test]
    fn removing_an_enrolment_reports_whether_it_was_there() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let id = Uuid::new_v4();
        put(&db, &enrolment(id, &[("vault", b"x")])).expect("put");

        assert!(remove(&db, id).expect("remove"), "an existing row removes to true");
        assert!(!remove(&db, id).expect("remove again"), "an absent row removes to false");
        assert!(get(&db, id).expect("get").is_none());
    }
}
