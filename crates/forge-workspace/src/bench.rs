//! Benching a model on this machine's own material: which models a run can
//! load, the config each one runs under, and the state and results the
//! page reads.
//!
//! The corpus and the runner are `forge-dictate`'s; this module is the
//! workspace's half - it knows the roles, the installed set and the store.

use std::path::Path;

use forge_dictate::ModelSpec;

use crate::dictate::DictateRole;
use crate::install::ActiveModel;

/// Which slot a bench target runs in, in the names the wire uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchRole {
    Transcribing,
    Cleanup,
}

/// One model a bench can load.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchTarget {
    pub file: String,
    pub role: BenchRole,
    /// Whether `forge.toml` pins the role this target would take - the
    /// page offers no activation for one, and a bench still runs.
    pub pinned: bool,
}

/// Every model this machine can bench, in the order the page lists them:
/// the roles' active models first, then the installed set as transcribing
/// candidates.
///
/// **A file that is not on disk is not a target.** The page's control
/// points at Install for one of those, because a bench cannot load what
/// was never downloaded and a run that failed on that would read as a
/// broken model rather than a missing file.
pub fn targets(
    models_dir: &Path,
    installed: &[crate::install::InstalledModel],
    active: &[(DictateRole, ActiveModel)],
) -> Vec<BenchTarget> {
    let mut rows: Vec<BenchTarget> = Vec::new();
    let on_disk = |file: &str| models_dir.join(file).is_file();

    for (role, model) in active {
        if !on_disk(&model.spec.file) {
            continue;
        }
        rows.push(BenchTarget {
            file: model.spec.file.clone(),
            role: role_for(*role),
            pinned: matches!(model.from, crate::install::ActiveFrom::Config { .. }),
        });
    }
    for model in installed {
        if rows.iter().any(|row| row.file == model.file) || !on_disk(&model.file) {
            continue;
        }
        rows.push(BenchTarget {
            file: model.file.clone(),
            role: BenchRole::Transcribing,
            pinned: false,
        });
    }
    rows
}

/// The bench's word for a role's slot.
pub fn role_for(role: DictateRole) -> BenchRole {
    match role {
        DictateRole::Transcribing => BenchRole::Transcribing,
        DictateRole::Normalization => BenchRole::Cleanup,
    }
}

/// The config one target runs under: the base with the target's file in
/// its role's slot.
///
/// The spec carries no URL and no digest - a bench loads what is already
/// on disk and never fetches - and the size is the file's own, so the
/// engine's check is against the bytes that are there.
pub fn config_for(
    base: &forge_dictate::Config,
    models_dir: &Path,
    target: &BenchTarget,
) -> Result<forge_dictate::Config, forge_dictate::Error> {
    let path = models_dir.join(&target.file);
    let size = std::fs::metadata(&path)
        .map_err(|source| forge_dictate::Error::Io { path: path.clone(), source })?
        .len();
    let spec = ModelSpec {
        file: target.file.clone(),
        url: String::new(),
        size,
        sha256: None,
        facts: forge_dictate::ModelFacts::default(),
    };

    let mut cfg = base.clone();
    match target.role {
        BenchRole::Transcribing => cfg.asr_model = spec,
        BenchRole::Cleanup => cfg.normalizer = Some(spec),
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{ActiveFrom, InstalledModel};

    fn spec(file: &str) -> ModelSpec {
        let mut spec = ModelSpec::cohere_transcribe_q4_k_m();
        spec.file = file.to_owned();
        spec
    }

    fn active(role: DictateRole, file: &str, from: ActiveFrom) -> (DictateRole, ActiveModel) {
        (role, ActiveModel { role, spec: spec(file), from, at: None })
    }

    fn installed(file: &str) -> InstalledModel {
        InstalledModel {
            variant: file.trim_end_matches(".gguf").to_owned(),
            file: file.to_owned(),
            url: format!("https://weights.invalid/{file}"),
            size: 6,
            facts: forge_dictate::ModelFacts::default(),
            at: "2026-10-06T00:00:00Z".to_owned(),
        }
    }

    /// A file present on disk, so the target rules are about the rules.
    fn present(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), b"weights").unwrap();
    }

    /// The pins are targets in both roles, and a config pin is marked as
    /// one.
    #[test]
    fn every_active_model_is_a_target_with_its_own_role() {
        let dir = tempfile::tempdir().unwrap();
        present(dir.path(), "asr.gguf");
        present(dir.path(), "cleanup.gguf");
        let active = vec![
            active(DictateRole::Transcribing, "asr.gguf", ActiveFrom::Pin),
            active(
                DictateRole::Normalization,
                "cleanup.gguf",
                ActiveFrom::Config {
                    key: "cleanup_model".to_owned(),
                    variant: "cleanup".to_owned(),
                },
            ),
        ];

        let rows = targets(dir.path(), &[], &active);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].role, BenchRole::Transcribing);
        assert!(!rows[0].pinned, "a compiled pin is not a config pin");
        assert_eq!(rows[1].role, BenchRole::Cleanup);
        assert!(rows[1].pinned, "the page must refuse an activation for that role");
    }

    /// The installed set is bench material too: an installed model that is
    /// not any role's active one is still a transcribing candidate.
    #[test]
    fn an_installed_model_is_a_target_and_the_active_one_is_not_listed_twice() {
        let dir = tempfile::tempdir().unwrap();
        present(dir.path(), "asr.gguf");
        present(dir.path(), "installed.gguf");
        present(dir.path(), "already.gguf");
        let active = vec![active(
            DictateRole::Transcribing,
            "already.gguf",
            ActiveFrom::Installed { variant: "already".to_owned() },
        )];
        let installed = vec![installed("installed.gguf"), installed("already.gguf")];

        let rows = targets(dir.path(), &installed, &active);

        assert_eq!(rows.len(), 2, "the active file is not listed twice: {rows:?}");
        assert_eq!(rows[1].file, "installed.gguf");
        assert_eq!(rows[1].role, BenchRole::Transcribing);
    }

    /// A file that is not on disk is not a target: the page points that row
    /// at Install instead, because a bench cannot load what is not there.
    #[test]
    fn a_file_that_is_not_on_disk_is_not_a_target() {
        let dir = tempfile::tempdir().unwrap();
        present(dir.path(), "asr.gguf");
        let active = vec![
            active(DictateRole::Transcribing, "asr.gguf", ActiveFrom::Pin),
            active(DictateRole::Normalization, "missing.gguf", ActiveFrom::Pin),
        ];

        let rows = targets(dir.path(), &[installed("also-missing.gguf")], &active);

        assert_eq!(rows.len(), 1, "only the file that is there: {rows:?}");
        assert_eq!(rows[0].file, "asr.gguf");
    }

    /// The config a target runs under: the base with the target's file in
    /// its slot, no URL and no digest - the bench fetches nothing - and the
    /// size the file's own.
    #[test]
    fn a_targets_config_swaps_its_own_slot_and_leaves_the_other_alone() {
        let dir = tempfile::tempdir().unwrap();
        present(dir.path(), "candidate.gguf");
        let base = forge_dictate::Config::default();

        let for_it = config_for(
            &base,
            dir.path(),
            &BenchTarget {
                file: "candidate.gguf".to_owned(),
                role: BenchRole::Transcribing,
                pinned: false,
            },
        )
        .expect("the file is on disk");

        assert_eq!(for_it.asr_model.file, "candidate.gguf");
        assert!(for_it.asr_model.url.is_empty(), "a bench never fetches");
        assert!(for_it.asr_model.sha256.is_none(), "no digest is published for one");
        assert_eq!(for_it.asr_model.size, 7, "the size is the bytes on disk");
        assert_eq!(
            for_it.normalizer.as_ref().map(|spec| spec.file.clone()),
            base.normalizer.as_ref().map(|spec| spec.file.clone()),
            "the other role keeps its model"
        );

        let for_cleanup = config_for(
            &base,
            dir.path(),
            &BenchTarget {
                file: "candidate.gguf".to_owned(),
                role: BenchRole::Cleanup,
                pinned: false,
            },
        )
        .expect("the file is on disk");
        assert_eq!(
            for_cleanup.normalizer.as_ref().map(|spec| spec.file.as_str()),
            Some("candidate.gguf")
        );
        assert_eq!(for_cleanup.asr_model.file, base.asr_model.file);
    }

    /// A target whose file vanished between the list and the run fails
    /// naming the path rather than loading something else.
    #[test]
    fn a_target_whose_file_is_gone_fails_by_name() {
        let dir = tempfile::tempdir().unwrap();

        let err = config_for(
            &forge_dictate::Config::default(),
            dir.path(),
            &BenchTarget {
                file: "gone.gguf".to_owned(),
                role: BenchRole::Transcribing,
                pinned: false,
            },
        )
        .expect_err("no file, no config");

        assert!(err.to_string().contains("gone.gguf"), "got: {err}");
    }
}
