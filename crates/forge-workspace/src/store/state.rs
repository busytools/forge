//! Machine-local forge state on the redb `settings` + `account_usage`
//! tables: the `/spinner` override (one `settings` row) and the
//! per-account usage cache (`account_usage`, one row per account). They
//! live in separate tables so the ~1/min usage churn never rewrites the
//! stable spinner preference. Values are serde-json; no field schema on
//! disk.

use std::collections::BTreeMap;

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};

use super::Db;
use crate::account_cache::CachedAccountUsage;
use crate::ui::SpinnerStyle;

const SETTINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("settings");
const ACCOUNT_USAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("account_usage");

/// The persisted `/spinner` override, or `None` when unset. An
/// undecodable value (a removed enum variant) resolves to `None` so the
/// forge.toml default wins rather than the whole read failing.
pub fn spinner(db: &Db) -> anyhow::Result<Option<SpinnerStyle>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SETTINGS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(value) = table.get("spinner")? else {
        return Ok(None);
    };
    match serde_json::from_slice::<SpinnerStyle>(value.value()) {
        Ok(style) => Ok(Some(style)),
        Err(err) => {
            tracing::warn!(
                target: "forge_workspace::store::state",
                error = %err,
                "ignoring an undecodable persisted spinner override",
            );
            Ok(None)
        }
    }
}

/// Persist the `/spinner` override. `None` clears the key so the active
/// style falls back to the forge.toml `[ui] spinner` default next boot.
pub fn set_spinner(db: &Db, spinner: Option<SpinnerStyle>) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SETTINGS)?;
        match spinner {
            Some(style) => {
                let value = serde_json::to_vec(&style).context("serialize spinner")?;
                table.insert("spinner", value.as_slice())?;
            }
            None => {
                table.remove("spinner")?;
            }
        }
    }
    txn.commit()?;
    Ok(())
}

/// Every cached per-account usage snapshot, keyed by account display
/// name. A record that fails to decode is skipped with a warn so one
/// corrupt blob can't wipe the rest of the cache.
pub fn account_usage(db: &Db) -> anyhow::Result<BTreeMap<String, CachedAccountUsage>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(ACCOUNT_USAGE) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty cache, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = BTreeMap::new();
    for entry in table.iter()? {
        let (name, value) = entry?;
        match serde_json::from_slice::<CachedAccountUsage>(value.value()) {
            Ok(usage) => {
                out.insert(name.value().to_owned(), usage);
            }
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::state",
                account = %name.value(),
                error = %err,
                "skipping account-usage record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Overwrite the usage cache with `entries`: clear every row then insert
/// the full map in one write txn, mirroring the poller's whole-map write.
pub fn replace_account_usage(
    db: &Db,
    entries: &BTreeMap<String, CachedAccountUsage>,
) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(ACCOUNT_USAGE)?;
        let existing: Vec<String> =
            table.iter()?.filter_map(Result::ok).map(|(k, _)| k.value().to_owned()).collect();
        for key in existing {
            table.remove(key.as_str())?;
        }
        for (name, entry) in entries {
            let value = serde_json::to_vec(entry).context("serialize account usage")?;
            table.insert(name.as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn usage_entry(utilization: f64) -> CachedAccountUsage {
        CachedAccountUsage {
            snapshot: UsageSnapshot {
                source: UsageSourceKind::Oauth,
                fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                five_hour: Some(UsageWindow {
                    utilization,
                    resets_at: None,
                    reset_description: None,
                }),
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            },
        }
    }

    fn usage_map(pairs: &[(&str, f64)]) -> BTreeMap<String, CachedAccountUsage> {
        pairs.iter().map(|(name, util)| ((*name).to_owned(), usage_entry(*util))).collect()
    }

    #[test]
    fn spinner_round_trips_and_clears() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        assert_eq!(spinner(&db).expect("read fresh"), None, "a fresh store has no override");

        set_spinner(&db, Some(SpinnerStyle::Ember)).expect("set");
        assert_eq!(
            spinner(&db).expect("read"),
            Some(SpinnerStyle::Ember),
            "the override round-trips"
        );

        set_spinner(&db, None).expect("clear");
        assert_eq!(spinner(&db).expect("read after clear"), None, "None clears the override");
    }

    #[test]
    fn account_usage_replace_mirrors_the_map() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        replace_account_usage(&db, &usage_map(&[("Granite", 42.0), ("Subspace", 10.0)]))
            .expect("first write");
        let loaded = account_usage(&db).expect("read");
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded
                .get("Granite")
                .and_then(|e| e.snapshot.five_hour.as_ref())
                .map(|w| w.utilization),
            Some(42.0),
        );

        // A second write drops the account no longer present.
        replace_account_usage(&db, &usage_map(&[("Granite", 99.0)])).expect("second write");
        let loaded = account_usage(&db).expect("read again");
        assert_eq!(loaded.len(), 1, "replace mirrors exactly the new map");
        assert!(!loaded.contains_key("Subspace"), "the dropped account is gone");
        assert_eq!(
            loaded
                .get("Granite")
                .and_then(|e| e.snapshot.five_hour.as_ref())
                .map(|w| w.utilization),
            Some(99.0),
            "the surviving account's snapshot is the new value",
        );
    }

    #[test]
    fn corrupt_account_usage_record_is_skipped_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        replace_account_usage(&db, &usage_map(&[("Granite", 42.0)])).expect("write good");

        // A blob that isn't a valid entry must not poison the load.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(ACCOUNT_USAGE).expect("open table");
            table.insert("corrupt", "not usage".as_bytes()).expect("insert corrupt");
        }
        txn.commit().expect("commit");

        let loaded = account_usage(&db).expect("read tolerates the corrupt blob");
        assert_eq!(loaded.len(), 1, "the good record survives a corrupt sibling");
        assert!(loaded.contains_key("Granite"));
    }

    #[test]
    fn undecodable_spinner_resolves_to_none() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        // A removed/renamed variant persisted by an older build.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(SETTINGS).expect("open table");
            table.insert("spinner", "\"forge_dot\"".as_bytes()).expect("insert bogus");
        }
        txn.commit().expect("commit");

        assert_eq!(
            spinner(&db).expect("read tolerates the bogus value"),
            None,
            "an undecodable spinner falls back to None, not an error",
        );
    }

    #[test]
    fn state_survives_db_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            set_spinner(&db, Some(SpinnerStyle::Star)).expect("set spinner");
            replace_account_usage(&db, &usage_map(&[("Granite", 42.0)])).expect("write usage");
        }
        let db = Db::open(&path).expect("reopen db");
        assert_eq!(
            spinner(&db).expect("read"),
            Some(SpinnerStyle::Star),
            "spinner survives restart"
        );
        assert!(
            account_usage(&db).expect("read").contains_key("Granite"),
            "the usage cache survives restart",
        );
    }

    #[test]
    fn account_usage_writes_leave_the_spinner_untouched() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            set_spinner(&db, Some(SpinnerStyle::Ember)).expect("set spinner");
            // A full usage-cache cycle. Two tables mean this never rewrites
            // the spinner row - the whole reason the lock could go.
            replace_account_usage(&db, &usage_map(&[("Granite", 42.0)])).expect("write usage");
            replace_account_usage(&db, &usage_map(&[("Subspace", 10.0)])).expect("churn usage");
            assert_eq!(
                spinner(&db).expect("read"),
                Some(SpinnerStyle::Ember),
                "usage churn leaves the spinner row untouched",
            );
        }
        let db = Db::open(&path).expect("reopen db");
        assert_eq!(
            spinner(&db).expect("read after reopen"),
            Some(SpinnerStyle::Ember),
            "the spinner survives usage churn across a restart",
        );
    }
}
