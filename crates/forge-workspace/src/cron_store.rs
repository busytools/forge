//! One-time migration reader for the legacy `<config_dir>/forge/cron.toml`.
//!
//! Crons now live in the machine-local store ([`crate::store::cron`]);
//! this module only reads the synced `cron.toml` once to seed that store
//! ([`load_crons`]). The whole module is removed in the cron-redb
//! follow-up, once every machine has migrated.
//!
//! Failures are non-fatal: a missing file loads empty, a corrupt file
//! loads empty + warns.

use std::path::{Path, PathBuf};

use forge_primitives::cron::CronEntry;
use serde::{Deserialize, Serialize};

const CRON_SCHEMA_VERSION: u8 = 1;
const CRON_FILE_NAME: &str = "cron.toml";

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
    crate::config::forge_data_dir(config_dir).join(CRON_FILE_NAME)
}

/// Legacy top-level cron path, read as a non-destructive fallback until
/// the file is moved under `forge/`. See [`load_crons`].
fn legacy_cron_path(config_dir: &Path) -> PathBuf {
    config_dir.join("forge-cron.toml")
}

/// Read cron entries at boot, preferring `forge/cron.toml` and falling
/// back to the legacy top-level `forge-cron.toml` (with a warn). Returns
/// an empty list on any failure (missing file, IO error, parse error,
/// schema-version mismatch). Writes always land under `forge/`; the
/// legacy copy is never removed here - that stays a manual post-rollout
/// step.
pub(crate) fn load_crons(config_dir: &Path) -> Vec<CronEntry> {
    let mut path = cron_path(config_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let legacy = legacy_cron_path(config_dir);
            match std::fs::read_to_string(&legacy) {
                Ok(s) => {
                    tracing::warn!(
                        target: "forge_workspace::cron_store",
                        legacy = %legacy.display(),
                        "crons read from the legacy top-level forge-cron.toml; written under forge/ from now on",
                    );
                    path = legacy;
                    s
                }
                Err(_) => return Vec::new(),
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::cron_store",
                error = %e,
                path = %path.display(),
                "cron.toml present but read failed; treating as empty",
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
                "cron.toml parse failed; treating as empty",
            );
            return Vec::new();
        }
    };
    if parsed.version != CRON_SCHEMA_VERSION {
        tracing::warn!(
            target: "forge_workspace::cron_store",
            disk_version = parsed.version,
            expected_version = CRON_SCHEMA_VERSION,
            "cron.toml schema-version mismatch; ignoring on-disk entries",
        );
        return Vec::new();
    }
    parsed.crons
}

/// Write a `cron.toml` fixture for the seed-migration tests. Production no
/// longer writes TOML (the machine-local store is the writer); this stays
/// only to build the legacy file [`load_crons`] reads.
#[cfg(test)]
pub(crate) fn store_crons(config_dir: &Path, crons: &[CronEntry]) {
    let doc = ForgeCronDoc { version: CRON_SCHEMA_VERSION, crons: crons.to_vec() };
    let path = cron_path(config_dir);
    let serialised = match toml::to_string_pretty(&doc) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::cron_store",
                error = %e,
                "cron.toml serialise failed; skipping write",
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
            "cron.toml tmp write failed",
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(
            target: "forge_workspace::cron_store",
            error = %e,
            from = %tmp_path.display(),
            to = %path.display(),
            "cron.toml atomic rename failed",
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

    /// A tempdir with `forge/` created, mirroring the boot-time
    /// `ensure_forge_data_dir` that guarantees the subfolder exists
    /// before any store writes into it.
    fn tmp() -> tempfile::TempDir {
        let d = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(d.path()).expect("forge/ dir");
        d
    }

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
            team_role: None,
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
            team_role: None,
        }
    }

    #[test]
    fn cron_path_is_under_forge_subfolder() {
        let dir = tmp();
        assert_eq!(cron_path(dir.path()), dir.path().join("forge").join("cron.toml"));
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let dir = tmp();
        assert!(load_crons(dir.path()).is_empty());
    }

    #[test]
    fn load_crons_falls_back_to_legacy_top_level() {
        let dir = tmp();
        let crons = vec![recurring("r-1")];
        // Seed forge/cron.toml, then relocate to the legacy top-level
        // path to simulate a pre-upgrade file (no forge/cron.toml).
        store_crons(dir.path(), &crons);
        std::fs::rename(cron_path(dir.path()), dir.path().join("forge-cron.toml"))
            .expect("relocate to legacy");
        assert!(!cron_path(dir.path()).exists());
        assert_eq!(load_crons(dir.path()), crons, "legacy top-level cron read via fallback");
    }

    #[test]
    fn prefers_forge_cron_over_legacy() {
        let dir = tmp();
        // Legacy has once("o-1"); forge/ has recurring("r-1"). forge/ wins.
        store_crons(dir.path(), &[once("o-1")]);
        std::fs::rename(cron_path(dir.path()), dir.path().join("forge-cron.toml")).expect("legacy");
        store_crons(dir.path(), &[recurring("r-1")]);
        let loaded = load_crons(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, CronId::from("r-1"), "forge/cron.toml wins over legacy");
    }

    #[test]
    fn round_trip_recurring_and_once() {
        let dir = tmp();
        let crons = vec![recurring("r-1"), once("o-1")];
        store_crons(dir.path(), &crons);
        assert_eq!(load_crons(dir.path()), crons, "both kinds survive a TOML round-trip");
    }

    /// A recurring cron whose prompt carries the multi-line + unicode
    /// shape real prompts use (em-dash, currency, accents, backticks,
    /// embedded newlines). Guards the TOML (de)serialize path against
    /// realistic content, not just ASCII fixtures. The em-dash is
    /// written as `\u{2014}` so it round-trips the real codepoint.
    #[test]
    fn round_trip_recurring_with_unicode_multiline_prompt() {
        let dir = tmp();
        let entry = CronEntry {
            id: CronId::from("r-unicode"),
            project_name: "trader-cc".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "Daily P&L \u{2014} summarise overnight fills.\n\
                     Budget \u{20AC}1.2M / \u{00A5}180M; watch the café é edge.\n\
                     Run `just report` then post `#trading`."
                .to_owned(),
            created_at: epoch(1_700_000_000),
            last_fire: Some(epoch(1_700_050_000)),
            next_fire: epoch(1_700_032_400),
            team_role: None,
        };
        store_crons(dir.path(), std::slice::from_ref(&entry));
        assert_eq!(
            load_crons(dir.path()),
            vec![entry],
            "a realistic multi-line unicode prompt survives the TOML round-trip intact",
        );
    }

    #[test]
    fn corrupt_toml_treated_as_empty() {
        let dir = tmp();
        std::fs::write(cron_path(dir.path()), "not = toml = at all").expect("write");
        assert!(load_crons(dir.path()).is_empty(), "a corrupt file loads empty, no panic");
    }

    #[test]
    fn version_mismatch_treated_as_empty() {
        let dir = tmp();
        std::fs::write(cron_path(dir.path()), "version = 9999\n").expect("write");
        assert!(load_crons(dir.path()).is_empty());
    }
}
