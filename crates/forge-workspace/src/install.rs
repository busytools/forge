//! Installing a model from the feed: its doc's own download link, a
//! size-checked fetch into the models directory, and the record of what
//! arrived.
//!
//! **A download is checked by the entry's own byte length, and by whether
//! the engine can load it - never by a digest, because upstream publishes
//! none for these weights.** Nothing here, in its copy or in its errors,
//! may call a downloaded file verified.

use std::cell::Cell;
use std::sync::Arc;

use forge_dictate::catalogue::{CatalogueEntry, CatalogueSource, doc_links, fetch_doc};
use forge_dictate::{ModelFacts, Progress};
use serde::{Deserialize, Serialize};

use crate::catalogue::PREFERRED_DOWNLOADS;
use crate::{DispatchError, SessionUpdate, Workspace};

/// Where an install is, as the page draws it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    #[default]
    Idle,
    /// `file` is named as soon as the doc says which file this is, so the
    /// page can say what is being fetched rather than a blank bar.
    Downloading { file: String, got: u64, total: u64 },
    Failed { file: String, reason: String },
}

/// One model this machine has downloaded from the feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledModel {
    /// The feed's own verb for the model, which is what a config key names.
    pub variant: String,
    pub file: String,
    /// The doc's own URL, kept so a reader can see where the bytes came from.
    pub url: String,
    pub size: u64,
    /// RFC 3339.
    pub at: String,
}

impl Workspace {
    /// The install's state, for the page's read.
    pub fn dictate_install(&self) -> InstallState {
        self.dictate_install.lock().clone()
    }

    /// Every model downloaded from the feed on this machine, oldest first.
    pub fn installed_models(&self) -> Vec<InstalledModel> {
        let db = self.db.lock();
        db.as_ref()
            .map(|db| crate::store::dictate_models::installed(db).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Install one feed variant: fetch its doc, take the quant the row
    /// draws, and download that file into the models directory.
    ///
    /// The outcome rides [`SessionUpdate::DictateModelsChanged`], the same
    /// way a catalogue check's does.
    pub(crate) fn start_install(self: &Arc<Self>, variant: String) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        {
            let mut state = self.dictate_install.lock();
            if matches!(*state, InstallState::Downloading { .. }) {
                return Err(DispatchError::Installing);
            }
            *state = InstallState::Downloading { file: String::new(), got: 0, total: 0 };
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let runner = Arc::clone(&this);
            runner.run_install(variant).await;
            this.push_models();
        });
        Ok(())
    }

    /// Take the install no further, naming the step that stopped.
    fn fail_install(&self, file: String, reason: String) {
        *self.dictate_install.lock() = InstallState::Failed { file, reason };
    }

    fn note_download(&self, file: &str, got: u64, total: u64) {
        *self.dictate_install.lock() =
            InstallState::Downloading { file: file.to_owned(), got, total };
    }

    /// Name the file while the doc is still being read, so the page's bar
    /// has something to say from the first tick.
    fn set_install_file(&self, file: &str, total: u64) {
        self.note_download(file, 0, total);
    }

    fn record_installed(&self, model: InstalledModel) {
        let db = self.db.lock();
        if let Some(db) = db.as_ref()
            && let Err(error) = crate::store::dictate_models::record_installed(db, &model)
        {
            tracing::warn!(
                event_name = "dictate_install_record_failed",
                %error,
                file = %model.file,
                "the downloaded model was not recorded; the next boot fetches it again"
            );
        }
    }

    /// The page's re-read, pushed the way a landed check pushes one.
    fn push_models(&self) {
        let models = self.dictate_models();
        let _ = self.update_sender().send(SessionUpdate::DictateModelsChanged { models });
    }

    /// The feed source and the entry a variant names, when both are known.
    fn variant_for_install(&self, variant: &str) -> Option<(CatalogueSource, CatalogueEntry)> {
        let source = self.catalogue_source();
        let state = self.dictate_catalogue.lock();
        let entry = state
            .catalogue
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.variant == variant)?
            .clone();
        Some((source, entry))
    }

    /// Fetch, check and record one variant. Every failure lands as an
    /// `InstallState::Failed` whose reason is its own, so the page says
    /// which step stopped rather than that something did.
    async fn run_install(self: Arc<Self>, variant: String) {
        let Some((source, entry)) = self.variant_for_install(&variant) else {
            return self.fail_install(String::new(), format!("no catalogue entry names {variant}"));
        };
        let Some(download) = preferred_download(&entry).cloned() else {
            return self.fail_install(
                String::new(),
                format!("{variant} ships no quantisation this machine would run"),
            );
        };

        let doc = {
            let source = source.clone();
            let variant = variant.clone();
            tokio::task::spawn_blocking(move || fetch_doc(&source, &variant)).await
        };
        let doc = match doc {
            Ok(Ok(doc)) => doc,
            Ok(Err(error)) => return self.fail_install(download.filename, error.to_string()),
            Err(join) => return self.fail_install(download.filename, join.to_string()),
        };
        let Some((_, url)) =
            doc_links(&doc).into_iter().find(|(file, _)| file == &download.filename)
        else {
            return self.fail_install(
                download.filename.clone(),
                format!("the doc carries no download for {}", download.filename),
            );
        };

        let spec = forge_dictate::spec_for_download(
            &download.filename,
            &url,
            download.size_bytes,
            ModelFacts {
                quant: Some(download.quant),
                params: Some(entry.params),
                license: entry.license.as_ref().map(|license| license.display.clone()),
                runtime: Some("transcribe.cpp".to_owned()),
            },
        );
        // The base is the `[dictate]` settings, with the candidate in the
        // ASR slot and no normalizer: `prepare` fetches what the config
        // names, and this config names one file.
        let Some(dir) = self.config.dictate.models_dir() else {
            return self.fail_install(
                download.filename,
                "no models directory is configured".to_owned(),
            );
        };
        let mut cfg = self.config.dictate.to_config();
        cfg.models_dir = Some(dir);
        cfg.asr_model = spec;
        cfg.normalizer = None;

        self.set_install_file(&download.filename, download.size_bytes);
        let this = Arc::clone(&self);
        let file = download.filename.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            // The push rides the whole-percent change: a tick per progress
            // event would re-encode the page's snapshot thousands of times
            // over a 279 MB file, and a percent is what a reader sees move.
            let last = Cell::new(u64::MAX);
            forge_dictate::prepare(&cfg, |progress| {
                if let Progress::Downloading { downloaded, total, .. } = progress {
                    let percent = if total == 0 { 0 } else { downloaded * 100 / total };
                    if percent != last.get() {
                        last.set(percent);
                        this.note_download(&file, downloaded, total);
                    }
                }
                std::ops::ControlFlow::Continue(())
            })
        })
        .await;

        match prepared {
            Ok(Ok(())) => {
                self.record_installed(InstalledModel {
                    variant,
                    file: download.filename,
                    url,
                    size: download.size_bytes,
                    at: rfc3339_now(),
                });
                *self.dictate_install.lock() = InstallState::Idle;
            }
            Ok(Err(error)) => self.fail_install(download.filename, error.to_string()),
            Err(join) => self.fail_install(download.filename, join.to_string()),
        }
    }
}

/// The download the row draws: the preferred quant chain, most wanted first.
fn preferred_download(entry: &CatalogueEntry) -> Option<&forge_dictate::catalogue::Download> {
    PREFERRED_DOWNLOADS
        .iter()
        .find_map(|quant| entry.downloads.iter().find(|download| &download.quant == quant))
}

/// A `SystemTime` as RFC 3339, the stamp a result is comparable by.
fn rfc3339_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests_install {
    use super::*;
    use crate::catalogue::DictateModelsSnapshot;
    use crate::catalogue::tests_catalogue_view::{
        enabled_stub, entry_under, serve_with, source,
    };
    use crate::{Command, DispatchError, SessionUpdate, Workspace};
    use forge_dictate::catalogue::Catalogue;

    /// One fetched feed carrying the given entries, as the install path
    /// reads it: the variant lookup goes through this state.
    fn catalogue_of(entries: Vec<forge_dictate::catalogue::CatalogueEntry>) -> Catalogue {
        Catalogue {
            fetched_at: "2026-10-06T00:00:00Z".to_owned(),
            release: None,
            entries,
            skipped: 0,
        }
    }

    /// A doc body shaped like the feed's own: the marked table carrying
    /// one quant row, its link pointing back at the loopback that serves it.
    fn doc_body(base: &str, variant: &str, file: &str) -> Vec<u8> {
        format!(
            "# {variant}\n\n<!-- catalog:downloads -->\n\
             | Quantization | Download | Size |\n\
             | --- | --- | ---: |\n\
             | Q4_K_M | [{file}]({base}/weights/{file}) | 6 B |\n\
             <!-- /catalog -->\n"
        )
        .into_bytes()
    }

    /// Everything a store-backed install test holds alive: the stub, its
    /// update feed, and the directories the models and the record land in.
    /// The plain `enabled_stub` keeps `db` at `None`, so recording is
    /// silently skipped there and a read-back test would pass vacuously.
    struct Fixture {
        ws: Arc<Workspace>,
        updates: tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
        _models: tempfile::TempDir,
        _store: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let (ws, updates, models) = enabled_stub();
        let store = tempfile::tempdir().expect("a store dir");
        ws.install_db_for_test(
            crate::store::Db::open(&store.path().join("db.redb")).expect("the store opens"),
        );
        Fixture { ws, updates, _models: models, _store: store }
    }

    /// The next re-read the install pushes, on the catalogue check's own
    /// fifteen-second budget.
    async fn await_models(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> DictateModelsSnapshot {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match updates.recv().await {
                    Some(SessionUpdate::DictateModelsChanged { models }) => break models,
                    Some(_) => {}
                    None => panic!("the subscription must stay attached"),
                }
            }
        })
        .await
        .expect("the install must land inside fifteen seconds")
    }

    /// A variant's doc names the file, the bytes arrive whole, and the
    /// model is recorded as installed: the record the next boot and the
    /// page's own reads both answer from.
    #[tokio::test]
    async fn an_install_lands_the_file_and_records_it() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                (format!("/docs/candidate.md"), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"111111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        assert!(
            matches!(landed.install, InstallState::Idle),
            "a clean install lands back at idle, got {:?}",
            landed.install
        );
        assert_eq!(landed.installed.len(), 1, "one file, one record");
        let record = &landed.installed[0];
        assert_eq!(record.variant, "candidate");
        assert_eq!(record.file, file);
        assert_eq!(record.url, format!("{base}/weights/{file}"), "the doc's own URL, kept");
        assert_eq!(record.size, 6);
        assert!(!record.at.is_empty(), "the record carries when it landed");
        assert_eq!(ws.installed_models().len(), 1, "and it reads back after the push");
    }

    /// Review Focus 2: bytes that disagree with the entry's own figure land
    /// failed, both numbers named, and nothing is recorded as installed.
    #[tokio::test]
    async fn an_install_whose_bytes_disagree_with_the_entry_lands_failed_and_records_nothing() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                (format!("/docs/candidate.md"), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"11111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        let InstallState::Failed { file: failed, reason } = &landed.install else {
            panic!("a size disagreement must land failed, got {:?}", landed.install);
        };
        assert_eq!(failed, file);
        assert!(
            reason.contains("5 bytes") && reason.contains("expected 6"),
            "the reason names both lengths, got: {reason}"
        );
        assert!(landed.installed.is_empty(), "nothing is recorded for a file that failed");
        assert!(ws.installed_models().is_empty());
    }

    /// A doc that carries no download for the quant the entry names - a
    /// table that moved, or one upstream trimmed - lands failed naming the
    /// file, rather than fetching some other row.
    #[tokio::test]
    async fn an_install_whose_doc_names_no_download_lands_failed() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|_| {
            vec![(String::from("/docs/candidate.md"), 200, b"# candidate\n\nprose only\n".to_vec())]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        let InstallState::Failed { file: failed, reason } = &landed.install else {
            panic!("a doc without the file must land failed, got {:?}", landed.install);
        };
        assert_eq!(failed, file, "the file the entry names, so the page says which one stopped");
        assert!(
            reason.contains("no download for"),
            "the reason names the step that stopped, got: {reason}"
        );
        assert!(ws.installed_models().is_empty());
    }

    /// Review Focus 5: with `[dictate]` off there is no models directory to
    /// install into and no engine to run one, so the command refuses by name.
    #[test]
    fn an_install_refuses_by_name_with_dictation_off() {
        let (ws, _updates) = Workspace::testing_stub();

        let err = ws
            .dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect_err("no dictation, no install");

        assert!(matches!(err, DispatchError::DictateOff), "got: {err:?}");
    }

    /// A second install while one is downloading is refused, and the one in
    /// flight is untouched: its progress bar is the answer.
    #[test]
    fn an_install_while_one_is_downloading_is_refused() {
        let (ws, _updates, _models) = enabled_stub();
        let in_flight =
            InstallState::Downloading { file: "half.gguf".to_owned(), got: 1, total: 6 };
        *ws.dictate_install.lock() = in_flight.clone();

        let err = ws
            .dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect_err("a second install must be refused");

        assert!(matches!(err, DispatchError::Installing), "got: {err:?}");
        assert_eq!(ws.dictate_install(), in_flight, "the refusal leaves the install in flight");
    }

    /// Installing the same file again replaces its record rather than
    /// stacking a second one: the set is one entry per file, which is what
    /// the page and the active-model resolution both read.
    #[tokio::test]
    async fn reinstalling_the_same_file_replaces_its_record() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                (format!("/docs/candidate.md"), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"111111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        let install = || Command::DictateInstall { variant: "candidate".to_owned() };

        ws.dispatch(install()).expect("the first install dispatches");
        let first = await_models(&mut updates).await;
        assert_eq!(first.installed.len(), 1);

        ws.dispatch(install()).expect("the second install dispatches");
        let second = await_models(&mut updates).await;

        assert_eq!(second.installed.len(), 1, "the record is replaced, not duplicated");
        assert_eq!(second.installed[0].file, file);
    }
}
