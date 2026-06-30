//! Durable forge-cron persistence: `<config_dir>/forge-cron.toml`.
//!
//! Mirrors [`crate::account_cache`]'s atomic tmp-file + rename writer.
//! Cross-process safety comes for free from the single-instance boot
//! guard ([`crate::single_instance`]): forge refuses to start a second
//! instance on the same config dir, so exactly one process ever touches
//! a given `forge-cron.toml`. That means an in-process `Mutex` is
//! enough here - no flock on the cron data, no separate cron lockfile.
//!
//! Reads ([`load_crons`]) take no lock: atomic rename means a writer
//! never exposes a torn file. The read-modify-write primitive
//! ([`with_cron_lock`]) holds the in-process mutex so a create / delete
//! and the scheduler's fire-advance can't interleave; it is what the
//! `cron__create` / `cron__delete` tools, the scheduler, and boot
//! catch-up build on.
//!
//! Failures are non-fatal: a missing file loads empty and a corrupt
//! file loads empty + warns.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forge_primitives::cron::CronEntry;
use serde::{Deserialize, Serialize};

const CRON_SCHEMA_VERSION: u8 = 1;
const CRON_FILE_RELATIVE_PATH: &str = "forge-cron.toml";

/// Versioned on-disk document. A `version` mismatch on read resets to
/// empty so a future schema change degrades to "no crons loaded" rather
/// than a parse panic.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ForgeCronDoc {
    version: u8,
    #[serde(default)]
    crons: Vec<CronEntry>,
}

impl ForgeCronDoc {
    fn empty() -> Self {
        Self { version: CRON_SCHEMA_VERSION, crons: Vec::new() }
    }
}

fn cron_path(config_dir: &Path) -> PathBuf {
    config_dir.join(CRON_FILE_RELATIVE_PATH)
}

/// Serializes the load-mutate-write cycle within this process. The
/// single-instance guard already rules out a second forge, so this is
/// the only coordination the cron store needs.
static CRON_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Read forge-cron.toml. Returns an empty list on any failure (missing
/// file, IO error, parse error, schema-version mismatch). No lock: the
/// atomic-rename writer never exposes a torn file.
pub(crate) fn load_crons(config_dir: &Path) -> Vec<CronEntry> {
    read_doc(config_dir).crons
}

fn read_doc(config_dir: &Path) -> ForgeCronDoc {
    let path = cron_path(config_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ForgeCronDoc::empty(),
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::cron_store",
                error = %e,
                path = %path.display(),
                "forge-cron.toml present but read failed; treating as empty",
            );
            return ForgeCronDoc::empty();
        }
    };
    let parsed: ForgeCronDoc = match toml::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::cron_store",
                error = %e,
                path = %path.display(),
                "forge-cron.toml parse failed; treating as empty",
            );
            return ForgeCronDoc::empty();
        }
    };
    if parsed.version != CRON_SCHEMA_VERSION {
        tracing::warn!(
            target: "forge_workspace::cron_store",
            disk_version = parsed.version,
            expected_version = CRON_SCHEMA_VERSION,
            "forge-cron.toml schema-version mismatch; ignoring on-disk entries",
        );
        return ForgeCronDoc::empty();
    }
    parsed
}

/// Run `mutate` against the current cron list with the write lock held,
/// then persist the result - the whole load-mutate-store cycle is atomic
/// against other threads in this process. Every cron write routes
/// through here: `cron__create` pushes, `cron__delete` retains, the
/// scheduler advances/removes a fired entry, and boot catch-up reconciles
/// the whole list.
pub(crate) fn with_cron_lock<R>(
    config_dir: &Path,
    mutate: impl FnOnce(&mut Vec<CronEntry>) -> R,
) -> R {
    let _guard = CRON_WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut crons = read_doc(config_dir).crons;
    let result = mutate(&mut crons);
    write_doc(config_dir, &ForgeCronDoc { version: CRON_SCHEMA_VERSION, crons });
    result
}

/// Serialise `doc` to `<config_dir>/forge-cron.toml` via tmp-file +
/// atomic rename: a crash between write and rename leaves the previous
/// document intact rather than a partial file. Failures are non-fatal
/// and logged at warn.
fn write_doc(config_dir: &Path, doc: &ForgeCronDoc) {
    let path = cron_path(config_dir);
    let serialised = match toml::to_string_pretty(doc) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::cron_store",
                error = %e,
                "forge-cron.toml serialise failed; skipping write",
            );
            return;
        }
    };
    let tmp_path = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp_path, &serialised) {
        tracing::warn!(
            target: "forge_workspace::cron_store",
            error = %e,
            path = %tmp_path.display(),
            "forge-cron.toml tmp write failed",
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(
            target: "forge_workspace::cron_store",
            error = %e,
            from = %tmp_path.display(),
            to = %path.display(),
            "forge-cron.toml atomic rename failed",
        );
        let _ = std::fs::remove_file(&tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::cron::{CronId, CronKind};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn recurring(id: &str) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: epoch(1_700_000_000),
            last_fire: None,
            next_fire: epoch(1_700_032_400),
        }
    }

    fn once(id: &str) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "busymail".to_owned(),
            kind: CronKind::Once(epoch(1_700_100_000)),
            prompt: "deploy".to_owned(),
            created_at: epoch(1_700_000_000),
            last_fire: Some(epoch(1_700_050_000)),
            next_fire: epoch(1_700_100_000),
        }
    }

    /// Seed the file by routing a full replacement through the write
    /// primitive - the same path prod uses, exercised in tests.
    fn seed(dir: &Path, crons: Vec<CronEntry>) {
        with_cron_lock(dir, |existing| *existing = crons);
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        assert!(load_crons(dir.path()).is_empty());
    }

    #[test]
    fn round_trip_recurring_and_once() {
        let dir = tempdir().expect("tempdir");
        let crons = vec![recurring("r-1"), once("o-1")];
        seed(dir.path(), crons.clone());

        let loaded = load_crons(dir.path());
        assert_eq!(loaded, crons, "both kinds survive a TOML round-trip");
    }

    #[test]
    fn corrupt_toml_treated_as_empty() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(cron_path(dir.path()), "not = toml = at all").expect("write");
        assert!(load_crons(dir.path()).is_empty(), "a corrupt file loads empty, no panic");
    }

    #[test]
    fn version_mismatch_treated_as_empty() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(cron_path(dir.path()), "version = 9999\n").expect("write");
        assert!(load_crons(dir.path()).is_empty());
    }

    #[test]
    fn write_uses_atomic_rename_and_leaves_no_tmp_file() {
        let dir = tempdir().expect("tempdir");
        seed(dir.path(), vec![recurring("r-1")]);

        let canonical = cron_path(dir.path());
        assert!(canonical.exists(), "canonical cron file present");
        assert!(
            !canonical.with_extension("toml.tmp").exists(),
            "tmp suffix file cleaned up after atomic rename",
        );
    }

    #[test]
    fn full_replace_overwrites_existing_file() {
        let dir = tempdir().expect("tempdir");
        seed(dir.path(), vec![recurring("r-1")]);
        seed(dir.path(), vec![once("o-1")]);

        let loaded = load_crons(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, CronId::from("o-1"), "the second write replaced the first");
    }

    #[test]
    fn with_cron_lock_read_modify_write_appends() {
        let dir = tempdir().expect("tempdir");
        seed(dir.path(), vec![recurring("r-1")]);
        with_cron_lock(dir.path(), |crons| crons.push(once("o-1")));

        let loaded = load_crons(dir.path());
        assert_eq!(loaded.len(), 2, "with_cron_lock preserved the seed and added the new entry");
    }

    /// Two threads hammering `with_cron_lock` concurrently: every
    /// distinct entry must survive. The in-process mutex makes each
    /// read-modify-write atomic, so the file ends with all 200 entries
    /// instead of losing updates to an interleave.
    #[test]
    fn concurrent_with_cron_lock_loses_no_updates() {
        use std::thread;
        let dir = tempdir().expect("tempdir");

        let p1 = dir.path().to_path_buf();
        let h1 = thread::spawn(move || {
            for i in 0..100 {
                with_cron_lock(&p1, |crons| crons.push(recurring(&format!("a-{i}"))));
            }
        });
        let p2 = dir.path().to_path_buf();
        let h2 = thread::spawn(move || {
            for i in 0..100 {
                with_cron_lock(&p2, |crons| crons.push(once(&format!("b-{i}"))));
            }
        });
        h1.join().expect("thread a");
        h2.join().expect("thread b");

        let loaded = load_crons(dir.path());
        assert_eq!(loaded.len(), 200, "no read-modify-write lost an update under the lock");
    }
}
