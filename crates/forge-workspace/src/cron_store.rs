//! Durable forge-cron persistence: `<config_dir>/forge-cron.toml`.
//!
//! The `Workspace` holds the cron list in memory (`Mutex<Vec<CronEntry>>`)
//! as the working source of truth; this module loads it at boot
//! ([`load_crons`]) and persists it after each mutation ([`store_crons`])
//! via an atomic tmp-file + rename, mirroring [`crate::account_cache`].
//!
//! No file lock lives here: the single-instance boot guard
//! ([`crate::single_instance`]) guarantees one forge process per config
//! dir, and the workspace's `crons` mutex serialises writes within that
//! process, so `store_crons` is always called under that lock.
//!
//! Failures are non-fatal: a missing file loads empty, a corrupt file
//! loads empty + warns.

use std::path::{Path, PathBuf};

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

fn cron_path(config_dir: &Path) -> PathBuf {
    config_dir.join(CRON_FILE_RELATIVE_PATH)
}

/// Read forge-cron.toml at boot. Returns an empty list on any failure
/// (missing file, IO error, parse error, schema-version mismatch).
pub(crate) fn load_crons(config_dir: &Path) -> Vec<CronEntry> {
    let path = cron_path(config_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::cron_store",
                error = %e,
                path = %path.display(),
                "forge-cron.toml present but read failed; treating as empty",
            );
            return Vec::new();
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
            return Vec::new();
        }
    };
    if parsed.version != CRON_SCHEMA_VERSION {
        tracing::warn!(
            target: "forge_workspace::cron_store",
            disk_version = parsed.version,
            expected_version = CRON_SCHEMA_VERSION,
            "forge-cron.toml schema-version mismatch; ignoring on-disk entries",
        );
        return Vec::new();
    }
    parsed.crons
}

/// Persist the current cron list to forge-cron.toml via tmp-file +
/// atomic rename: a crash between write and rename leaves the previous
/// document intact rather than a partial file. Called under the
/// workspace's `crons` mutex. Failures are non-fatal and logged at warn.
pub(crate) fn store_crons(config_dir: &Path, crons: &[CronEntry]) {
    let doc = ForgeCronDoc { version: CRON_SCHEMA_VERSION, crons: crons.to_vec() };
    let path = cron_path(config_dir);
    let serialised = match toml::to_string_pretty(&doc) {
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
            project_name: "airmail".to_owned(),
            kind: CronKind::Once(epoch(1_700_100_000)),
            prompt: "deploy".to_owned(),
            created_at: epoch(1_700_000_000),
            last_fire: Some(epoch(1_700_050_000)),
            next_fire: epoch(1_700_100_000),
        }
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
        store_crons(dir.path(), &crons);
        assert_eq!(load_crons(dir.path()), crons, "both kinds survive a TOML round-trip");
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
    fn store_uses_atomic_rename_and_leaves_no_tmp_file() {
        let dir = tempdir().expect("tempdir");
        store_crons(dir.path(), &[recurring("r-1")]);

        let canonical = cron_path(dir.path());
        assert!(canonical.exists(), "canonical cron file present");
        assert!(
            !canonical.with_extension("toml.tmp").exists(),
            "tmp suffix file cleaned up after atomic rename",
        );
    }

    #[test]
    fn store_overwrites_existing_file() {
        let dir = tempdir().expect("tempdir");
        store_crons(dir.path(), &[recurring("r-1")]);
        store_crons(dir.path(), &[once("o-1")]);

        let loaded = load_crons(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, CronId::from("o-1"), "the second write replaced the first");
    }
}
