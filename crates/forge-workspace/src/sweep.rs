//! The weekly exploration sweep: the feed's candidates, the budget, the
//! tested state, and the load smoke that stands between a download and a
//! bench.
//!
//! **The feed is the compatibility index.** Every entry names a published
//! Hugging Face repo and the quantisations transcribe.cpp can load, so a
//! feed candidate is known-compatible before it is downloaded. A direct
//! Hugging Face hit has no such contract: it is marked unverified, and it
//! must pass [`smoke_gate`] before it may be benched.
//!
//! Nothing here writes the live redb. The sweep reads the live roles off a
//! copy (through [`crate::bench::resolved_live_roles`]) and keeps its own
//! state under the bench cache; the server owns the store.

use std::path::{Path, PathBuf};

use forge_dictate::catalogue::{CatalogueEntry, CatalogueSource, Download, EntryKind};
use forge_dictate::{Config, ModelFacts, ModelSpec};
use serde::{Deserialize, Serialize};

/// The directory the sweep downloads into: never the models directory.
pub fn bench_cache_dir(app_support: &Path) -> PathBuf {
    app_support.join("dictate-bench")
}

/// The tested-state file under the bench cache.
pub const TESTED_FILE: &str = "tested.json";

/// The results file under the bench cache.
pub const RESULTS_FILE: &str = "results.json";

/// One `(variant, quant)` the sweep has already tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestedPair {
    pub variant: String,
    pub quant: String,
    /// RFC 3339.
    pub at: String,
    /// `benched` or `unloadable`.
    pub outcome: String,
}

/// The pairs the sweep has tried, as the tested-state file holds them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestedState {
    pub pairs: Vec<TestedPair>,
}

impl TestedState {
    /// Whether this exact pair has already been tried.
    pub fn has(&self, variant: &str, quant: &str) -> bool {
        self.pairs.iter().any(|pair| pair.variant == variant && pair.quant == quant)
    }

    /// Record one tried pair, replacing a row for the same pair.
    pub fn record(&mut self, pair: TestedPair) {
        self.pairs.retain(|held| held.variant != pair.variant || held.quant != pair.quant);
        self.pairs.push(pair);
    }
}

/// Read the tested state under `dir`. A missing or unreadable file is an
/// empty state: a first run has nothing to diff against, and a corrupt file
/// must not stop every later sweep.
pub fn load_tested(dir: &Path) -> TestedState {
    let path = dir.join(TESTED_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return TestedState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Write the tested state under `dir`, beside and renamed over so a process
/// that dies mid-write leaves the previous state standing.
pub fn save_tested(dir: &Path, state: &TestedState) -> Result<(), String> {
    write_json(dir, TESTED_FILE, state)
}

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// A transcribe.cpp catalogue entry: the runtime's own compatibility
    /// index, so the family and quant are known to load.
    Feed,
    /// A direct Hugging Face hit: no compatibility contract, and it must
    /// pass the load smoke before it may be benched.
    Direct,
}

/// One candidate the sweep will try.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub source: CandidateSource,
    pub variant: String,
    pub display_name: String,
    pub family: String,
    pub quant: String,
    pub file: String,
    pub size: u64,
    pub sha256: Option<String>,
    /// The file's own URL when the feed names one. A speech-feed entry
    /// leaves this `None`; [`Candidate::spec`] reads its documents' link
    /// table at download time.
    pub url: Option<String>,
    pub published_repo: Option<String>,
    /// The feed's best m4-max Metal wall-clock realtime factor.
    pub speed: Option<f64>,
    /// The newest date across the feed's speed rows, `YYYY-MM-DD`.
    pub measured_on: Option<String>,
    pub download_count: Option<u64>,
}

impl Candidate {
    /// The spec the download and the engine read. A candidate that names
    /// its URL uses it; a speech-feed candidate reads the variant's own
    /// documents for the link that carries its file.
    pub fn spec(&self, source: &CatalogueSource) -> Result<ModelSpec, String> {
        let url = if let Some(url) = &self.url {
            url.clone()
        } else {
            let links = forge_dictate::catalogue::download_links(
                source,
                &self.variant,
                self.published_repo.as_deref(),
            )
            .map_err(|error| error.to_string())?;
            let file = &self.file;
            links
                .into_iter()
                .find(|(candidate, _)| candidate == file)
                .map(|(_, url)| url)
                .ok_or_else(|| format!("the feed's documents carry no download for {file}"))?
        };
        Ok(forge_dictate::spec_for_download(
            &self.file,
            &url,
            self.size,
            self.sha256.clone(),
            ModelFacts {
                quant: Some(self.quant.clone()),
                params: None,
                license: None,
                // Every ASR the sweep tries runs on transcribe.cpp; the feed's
                // normalizer entries never reach here.
                runtime: Some("transcribe.cpp".to_owned()),
            },
        ))
    }
}

/// One candidate that was not taken, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub variant: String,
    pub quant: String,
    pub reason: String,
}

/// What the discovery picked, and what it left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
    /// The bytes the selected candidates download together.
    pub bytes: u64,
}

/// The preferred quantisation a row draws: the first the entry ships, in
/// the runtime's own preference order.
fn preferred_download(entry: &CatalogueEntry) -> Option<&Download> {
    forge_dictate::catalogue::PREFERRED_DOWNLOADS
        .iter()
        .find_map(|quant| entry.downloads.iter().find(|download| &download.quant == quant))
}

/// The newest date the feed measured this entry's speed rows on.
fn newest_measured_on(entry: &CatalogueEntry) -> Option<String> {
    entry.speed_benchmarks.iter().filter_map(|row| row.measured_on.clone()).max()
}

/// Whether the entry names English, or names no language at all. An empty
/// list is unknown rather than "not English", so it stays eligible.
fn speaks_english(entry: &CatalogueEntry) -> bool {
    entry.languages.is_empty() || entry.languages.iter().any(|language| language == "en")
}

/// Pick at most `max` ASR candidates not in use, not already tested, with a
/// preferred download whose combined size is within `budget_bytes`.
///
/// **The ranking is the feed's own evidence**: fastest m4-max Metal row
/// first, the newest measurement breaking a tie, then the Hub's download
/// count. A stable sort keeps the feed's order beyond that.
pub fn select_candidates(
    entries: &[CatalogueEntry],
    tested: &TestedState,
    in_use_files: &[String],
    budget_bytes: u64,
    max: usize,
) -> Selection {
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();

    for entry in entries {
        if entry.kind != EntryKind::Asr {
            continue;
        }
        // The speech feed also lists voice-activity and language-id models;
        // `kind` calls them speech models, and only this flag says whether a
        // transcript can come out of one.
        if !entry.capabilities.transcribe.supported {
            skipped.push(Skipped {
                variant: entry.variant.clone(),
                quant: String::new(),
                reason: "does not transcribe".to_owned(),
            });
            continue;
        }
        let Some(download) = preferred_download(entry) else {
            skipped.push(Skipped {
                variant: entry.variant.clone(),
                quant: String::new(),
                reason: "no preferred quantisation".to_owned(),
            });
            continue;
        };
        if download.filename.is_empty() || download.size_bytes == 0 {
            skipped.push(Skipped {
                variant: entry.variant.clone(),
                quant: download.quant.clone(),
                reason: "the feed names no file or byte length".to_owned(),
            });
            continue;
        }
        if tested.has(&entry.variant, &download.quant) {
            skipped.push(Skipped {
                variant: entry.variant.clone(),
                quant: download.quant.clone(),
                reason: "already tested".to_owned(),
            });
            continue;
        }
        if in_use_files.iter().any(|file| file == &download.filename) {
            skipped.push(Skipped {
                variant: entry.variant.clone(),
                quant: download.quant.clone(),
                reason: "in use".to_owned(),
            });
            continue;
        }
        if !speaks_english(entry) {
            skipped.push(Skipped {
                variant: entry.variant.clone(),
                quant: download.quant.clone(),
                reason: "not an English model".to_owned(),
            });
            continue;
        }
        eligible.push(Candidate {
            source: CandidateSource::Feed,
            variant: entry.variant.clone(),
            display_name: entry.display_name.clone(),
            family: entry.family.clone(),
            quant: download.quant.clone(),
            file: download.filename.clone(),
            size: download.size_bytes,
            sha256: download.sha256.clone(),
            url: download.url.clone(),
            published_repo: entry.published_repo.clone(),
            speed: entry.m4_metal_xrt_wall(),
            measured_on: newest_measured_on(entry),
            download_count: entry.download_count,
        });
    }

    eligible.sort_by(|a, b| {
        b.speed
            .unwrap_or(0.0)
            .total_cmp(&a.speed.unwrap_or(0.0))
            .then_with(|| b.measured_on.cmp(&a.measured_on))
            .then_with(|| b.download_count.unwrap_or(0).cmp(&a.download_count.unwrap_or(0)))
    });

    let mut selection = Selection { candidates: Vec::new(), skipped, bytes: 0 };
    for candidate in eligible {
        if selection.candidates.len() >= max {
            selection.skipped.push(Skipped {
                variant: candidate.variant,
                quant: candidate.quant,
                reason: "beyond the run's candidate cap".to_owned(),
            });
            continue;
        }
        if selection.bytes.saturating_add(candidate.size) > budget_bytes {
            selection.skipped.push(Skipped {
                variant: candidate.variant,
                quant: candidate.quant,
                reason: "the run's budget is spent".to_owned(),
            });
            continue;
        }
        selection.bytes += candidate.size;
        selection.candidates.push(candidate);
    }
    selection
}

/// One direct Hugging Face hit: a GGUF file in a repo whose family the feed
/// already supports. **Unverified**: the repo carries no compatibility
/// contract with transcribe.cpp.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectHit {
    pub repo: String,
    pub file: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub downloads: Option<u64>,
    pub quant: String,
}

impl DirectHit {
    /// The candidate a direct hit describes, marked unverified.
    pub fn candidate(&self) -> Candidate {
        let repo = &self.repo;
        let file = &self.file;
        Candidate {
            source: CandidateSource::Direct,
            variant: self.repo.clone(),
            display_name: self.repo.clone(),
            family: String::new(),
            quant: self.quant.clone(),
            file: self.file.clone(),
            size: self.size,
            sha256: self.sha256.clone(),
            url: Some(format!("https://huggingface.co/{repo}/resolve/main/{file}")),
            published_repo: Some(self.repo.clone()),
            speed: None,
            measured_on: None,
            download_count: self.downloads,
        }
    }
}

/// The GGUF files one Hub repo publishes, restricted to the quants this
/// runtime runs. The first preferred quant a repo ships is the one it is
/// tried at.
pub fn direct_hits(
    repo: &str,
    downloads: Option<u64>,
    siblings: &serde_json::Value,
) -> Vec<DirectHit> {
    let Some(files) = siblings.as_array() else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for file in files {
        let Some(name) = file.get("rfilename").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let lower = name.to_lowercase();
        if !std::path::Path::new(&lower)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
        {
            continue;
        }
        let Some(quant) = forge_dictate::catalogue::PREFERRED_DOWNLOADS
            .iter()
            .find(|quant| lower.contains(quant.to_lowercase().as_str()))
        else {
            continue;
        };
        let size = file
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                file.get("lfs").and_then(|lfs| lfs.get("size")).and_then(serde_json::Value::as_u64)
            })
            .unwrap_or(0);
        if size == 0 {
            continue;
        }
        let sha256 = file
            .get("lfs")
            .and_then(|lfs| lfs.get("sha256"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        hits.push(DirectHit {
            repo: repo.to_owned(),
            file: name.to_owned(),
            size,
            sha256,
            downloads,
            quant: (*quant).to_owned(),
        });
    }
    hits
}

/// Pick direct hits under the same tested-state, budget and cap rules as the
/// feed. One candidate per repo, newest name first, and the Hub's download
/// count orders them because a direct hit has no measured speed.
pub fn select_direct(
    hits: &[DirectHit],
    tested: &TestedState,
    budget_bytes: u64,
    max: usize,
) -> Selection {
    let mut eligible: Vec<Candidate> = hits
        .iter()
        .filter(|hit| !tested.has(&hit.repo, &hit.quant))
        .map(DirectHit::candidate)
        .collect();
    eligible.sort_by(|a, b| {
        b.download_count
            .unwrap_or(0)
            .cmp(&a.download_count.unwrap_or(0))
            .then_with(|| a.variant.cmp(&b.variant))
    });

    let mut selection = Selection::default();
    for candidate in eligible {
        if selection.candidates.iter().any(|held| held.variant == candidate.variant) {
            continue;
        }
        if selection.candidates.len() >= max {
            selection.skipped.push(Skipped {
                variant: candidate.variant,
                quant: candidate.quant,
                reason: "beyond the run's candidate cap".to_owned(),
            });
            continue;
        }
        if selection.bytes.saturating_add(candidate.size) > budget_bytes {
            selection.skipped.push(Skipped {
                variant: candidate.variant,
                quant: candidate.quant,
                reason: "the run's budget is spent".to_owned(),
            });
            continue;
        }
        selection.bytes += candidate.size;
        selection.candidates.push(candidate);
    }
    selection
}

/// Query the Hub for GGUF repos in the families the feed already supports.
///
/// A fallback only: the feed stays the recommendation source. Each repo is
/// read for its file list, and every hit it yields is unverified.
pub fn search_hugging_face(
    families: &[String],
    per_family: usize,
) -> Result<Vec<DirectHit>, String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("forge-workspace/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("the Hub client did not build: {error}"))?;

    let mut hits = Vec::new();
    for family in families {
        if family.is_empty() {
            continue;
        }
        let search = format!(
            "https://huggingface.co/api/models?search={family}&filter=gguf&limit={per_family}"
        );
        let Some(models) = get_json(&client, &search) else {
            continue;
        };
        let Some(models) = models.as_array() else {
            continue;
        };
        for model in models {
            let Some(repo) = model.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let downloads = model.get("downloads").and_then(serde_json::Value::as_u64);
            let detail = format!("https://huggingface.co/api/models/{repo}?blobs=true");
            let Some(detail) = get_json(&client, &detail) else {
                continue;
            };
            let Some(siblings) = detail.get("siblings") else {
                continue;
            };
            hits.extend(direct_hits(repo, downloads, siblings));
        }
    }
    Ok(hits)
}

/// One Hub response as JSON, or `None` when it cannot be read. A search
/// family that fails costs its rows, not the sweep.
fn get_json(client: &reqwest::blocking::Client, url: &str) -> Option<serde_json::Value> {
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().ok()
}

/// Download one candidate into `dir`, with the size and digest checks its
/// spec carries. The path is the file the bench then loads.
///
/// [`forge_dictate::prepare`] is the install path's own fetch: it writes
/// into the config's `models_dir`, checks the spec's byte length, and
/// verifies the digest where the feed carries one.
pub fn download_candidate(
    candidate: &Candidate,
    spec: &ModelSpec,
    dir: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let cfg = Config {
        models_dir: Some(dir.to_path_buf()),
        asr_model: spec.clone(),
        normalizer: None,
        diagnostics_dir: None,
        digest_cache_dir: None,
        ..Config::default()
    };
    forge_dictate::prepare(&cfg, |_progress| std::ops::ControlFlow::Continue(()))
        .map_err(|error| error.to_string())?;
    Ok(dir.join(&candidate.file))
}

/// Load and transcribe one short clip through the config's engine. This is
/// the gate a direct Hugging Face hit must pass before it may be benched.
pub fn load_smoke(cfg: &Config, clip: &[f32]) -> Result<(), String> {
    let engine = forge_dictate::Engine::new(cfg.clone()).map_err(|error| error.to_string())?;
    engine.wait_ready().map_err(|error| error.to_string())?;
    let outcome = engine
        .transcribe(forge_dictate::Samples::mono(clip.to_vec()))
        .and_then(forge_dictate::Ticket::recv)
        .map_err(|error| error.to_string())?;
    match outcome {
        forge_dictate::Outcome::Transcript(_) => Ok(()),
        other @ forge_dictate::Outcome::NoAudio { .. } => {
            Err(format!("the clip produced no transcript: {other:?}"))
        }
    }
}

/// Run `smoke` and, when it fails, delete the downloaded file. A candidate
/// that will not load is reported as unloadable and gone, never benched.
pub fn smoke_gate(file: &Path, smoke: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    match smoke() {
        Ok(()) => Ok(()),
        Err(reason) => {
            let _ = std::fs::remove_file(file);
            Err(reason)
        }
    }
}

/// One tier's row in a model's table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierReport {
    pub tier: String,
    pub clips: usize,
    /// The tier's word error rate where its words are known, `None` on the
    /// saved takes.
    pub wer: Option<f64>,
    /// The mean word error rate against the takes' own recorded words,
    /// `None` on a tier whose clips carry no model text.
    pub mean_divergence: Option<f64>,
    pub xrt_wall: f64,
}

/// One take where a run's words differ from the in-use model's, with both
/// texts so the difference can be judged by ear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disagreement {
    pub clip: usize,
    pub candidate: String,
    pub in_use: String,
}

/// One model's table: its tiers, the takes it disagreed on, and the
/// one-model answer when the run is a no-normalizer comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelReport {
    pub label: String,
    /// `live`, `feed`, or `direct`.
    pub source: String,
    pub tiers: Vec<TierReport>,
    pub disagreements: Vec<Disagreement>,
    pub one_model: Option<String>,
}

/// One candidate's download outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadReport {
    pub variant: String,
    pub quant: String,
    pub file: String,
    pub size: u64,
    pub sha256: Option<String>,
    /// `benched` or `unloadable`.
    pub outcome: String,
    pub reason: Option<String>,
}

/// One run's whole result, written under the bench cache.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SweepReport {
    /// RFC 3339.
    pub at: String,
    pub feed_release: Option<String>,
    pub feed_skipped: usize,
    pub budget_bytes: u64,
    pub downloaded: Vec<DownloadReport>,
    pub models: Vec<ModelReport>,
    /// The candidates that beat the live pair, named as proposals. Nothing
    /// here switches a role.
    pub proposed: Vec<String>,
}

/// Write one report under `dir`, atomically.
pub fn write_report(dir: &Path, report: &SweepReport) -> Result<(), String> {
    write_json(dir, RESULTS_FILE, report)
}

/// Write one JSON file under `dir`, beside and renamed over.
fn write_json<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let body = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("{name} did not serialise: {error}"))?;
    let path = dir.join(name);
    let partial = dir.join(format!("{name}.tmp"));
    std::fs::write(&partial, body).map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, &path).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_dictate::catalogue::SpeedRow;

    fn entry(
        variant: &str,
        quant: &str,
        filename: &str,
        size: u64,
        speed: Option<f64>,
    ) -> CatalogueEntry {
        CatalogueEntry {
            variant: variant.to_owned(),
            display_name: variant.to_owned(),
            family: "test".to_owned(),
            params: None,
            license: None,
            languages: vec!["en".to_owned()],
            capabilities: forge_dictate::catalogue::Capabilities {
                transcribe: forge_dictate::catalogue::Support { supported: true },
                ..forge_dictate::catalogue::Capabilities::default()
            },
            headline_benchmark: None,
            published_repo: None,
            downloads: vec![Download {
                quant: quant.to_owned(),
                filename: filename.to_owned(),
                size_bytes: size,
                sha256: None,
                url: Some(format!("https://weights.invalid/{filename}")),
            }],
            speed_benchmarks: speed.map_or_else(Vec::new, |xrt| {
                vec![SpeedRow {
                    machine: "m4-max".to_owned(),
                    backend: "metal".to_owned(),
                    quant: quant.to_owned(),
                    xrt_wall: Some(xrt),
                    measured_on: Some("2026-11-01".to_owned()),
                }]
            }),
            accuracy_benchmarks: Vec::new(),
            kind: EntryKind::Asr,
            download_count: None,
        }
    }

    fn pair(variant: &str, quant: &str) -> TestedPair {
        TestedPair {
            variant: variant.to_owned(),
            quant: quant.to_owned(),
            at: "2026-11-01T00:00:00Z".to_owned(),
            outcome: "benched".to_owned(),
        }
    }

    /// **A tested pair is the diff's whole guard.** Without the skip, the
    /// same model would be downloaded and benched every week.
    #[test]
    fn a_tested_pair_is_not_selected_again() {
        let entries = vec![entry("a", "Q4_K_M", "a-Q4_K_M.gguf", 100, Some(10.0))];
        let mut tested = TestedState::default();
        tested.record(pair("a", "Q4_K_M"));

        let selection = select_candidates(&entries, &tested, &[], 1_000, 2);

        assert!(selection.candidates.is_empty(), "a tested pair is not offered again");
        assert!(
            selection.skipped.iter().any(|skip| skip.reason == "already tested"),
            "and it is named: {:?}",
            selection.skipped
        );
    }

    /// **A model in use is not a candidate.** The live ASR's own file is
    /// the one thing the sweep must never download over.
    #[test]
    fn a_file_in_use_is_not_selected() {
        let entries = vec![entry("a", "Q4_K_M", "a-Q4_K_M.gguf", 100, Some(10.0))];

        let selection = select_candidates(
            &entries,
            &TestedState::default(),
            &["a-Q4_K_M.gguf".to_owned()],
            1_000,
            2,
        );

        assert!(selection.candidates.is_empty());
        assert!(
            selection.skipped.iter().any(|skip| skip.reason == "in use"),
            "skipped: {:?}",
            selection.skipped
        );
    }

    /// **The budget is the run's total, not each candidate's.** A second
    /// candidate that would cross it is left out even when it alone fits.
    #[test]
    fn the_budget_caps_the_run() {
        let entries = vec![
            entry("a", "Q4_K_M", "a-Q4_K_M.gguf", 600, Some(20.0)),
            entry("b", "Q4_K_M", "b-Q4_K_M.gguf", 600, Some(10.0)),
        ];

        let selection = select_candidates(&entries, &TestedState::default(), &[], 1_000, 2);

        assert_eq!(selection.candidates.len(), 1, "only the first fits");
        assert!(selection.bytes <= 1_000);
        assert!(
            selection.skipped.iter().any(|skip| skip.reason == "the run's budget is spent"),
            "the excluded candidate names the budget"
        );
    }

    /// **At most two candidates a run.** The plan's weekly budget is two
    /// downloads, so a third eligible entry waits for the next sweep.
    #[test]
    fn the_cap_takes_at_most_two() {
        let entries = vec![
            entry("a", "Q4_K_M", "a-Q4_K_M.gguf", 10, Some(30.0)),
            entry("b", "Q4_K_M", "b-Q4_K_M.gguf", 10, Some(20.0)),
            entry("c", "Q4_K_M", "c-Q4_K_M.gguf", 10, Some(10.0)),
        ];

        let selection = select_candidates(&entries, &TestedState::default(), &[], 1_000, 2);

        assert_eq!(selection.candidates.len(), 2);
        assert_eq!(selection.candidates[0].variant, "a", "the fastest first");
        assert_eq!(selection.candidates[1].variant, "b");
    }

    /// **The ranking is the feed's speed rows, fastest first.** A sweep
    /// that picked feed order would try slow models before fast ones.
    #[test]
    fn the_fastest_candidate_is_selected_first() {
        let entries = vec![
            entry("slow", "Q4_K_M", "slow-Q4_K_M.gguf", 10, Some(10.0)),
            entry("fast", "Q4_K_M", "fast-Q4_K_M.gguf", 10, Some(90.0)),
        ];

        let selection = select_candidates(&entries, &TestedState::default(), &[], 1_000, 2);

        assert_eq!(selection.candidates[0].variant, "fast");
    }

    /// **The newest measurement breaks a speed tie.** The feed's recency is
    /// the only per-entry age it carries.
    #[test]
    fn the_newest_measurement_breaks_a_speed_tie() {
        let mut older = entry("older", "Q4_K_M", "older-Q4_K_M.gguf", 10, Some(10.0));
        if let Some(row) = older.speed_benchmarks.first_mut() {
            row.measured_on = Some("2026-10-01".to_owned());
        }
        let newer = entry("newer", "Q4_K_M", "newer-Q4_K_M.gguf", 10, Some(10.0));

        let selection = select_candidates(&[older, newer], &TestedState::default(), &[], 1_000, 2);

        assert_eq!(selection.candidates[0].variant, "newer");
    }

    /// A candidate whose entry ships no preferred quant is not a candidate:
    /// the sweep will not guess a quantisation the runtime may not run.
    #[test]
    fn a_candidate_with_no_preferred_quant_is_skipped() {
        let mut entry = entry("a", "F32", "a-F32.gguf", 10, Some(10.0));
        entry.downloads = vec![Download {
            quant: "F32".to_owned(),
            filename: "a-F32.gguf".to_owned(),
            size_bytes: 10,
            sha256: None,
            url: None,
        }];

        let selection = select_candidates(&[entry], &TestedState::default(), &[], 1_000, 2);

        assert!(selection.candidates.is_empty());
        assert!(
            selection.skipped.iter().any(|skip| skip.reason == "no preferred quantisation"),
            "skipped: {:?}",
            selection.skipped
        );
    }

    /// A normalizer entry is not an ASR candidate. The feed's own kind says
    /// which slot an entry fills.
    #[test]
    fn a_normalizer_is_not_an_asr_candidate() {
        let mut entry = entry("norm", "Q4_K_M", "norm-Q4_K_M.gguf", 10, Some(10.0));
        entry.kind = EntryKind::Normalizer;

        let selection = select_candidates(&[entry], &TestedState::default(), &[], 1_000, 2);

        assert!(selection.candidates.is_empty());
    }

    /// **A feed entry that cannot transcribe is not an ASR candidate.** The
    /// speech feed also lists voice-activity and language-id models; a
    /// sweep that downloaded one would spend its budget on a model that
    /// produces no transcript.
    #[test]
    fn a_non_transcribing_entry_is_skipped() {
        let mut entry = entry("lang-id", "Q8_0", "lang-id-Q8_0.gguf", 10, Some(10.0));
        entry.capabilities.transcribe = forge_dictate::catalogue::Support { supported: false };

        let selection = select_candidates(&[entry], &TestedState::default(), &[], 1_000, 2);

        assert!(selection.candidates.is_empty());
        assert!(
            selection.skipped.iter().any(|skip| skip.reason == "does not transcribe"),
            "skipped: {:?}",
            selection.skipped
        );
    }

    /// The tested state round-trips through its file, and a missing file is
    /// an empty state rather than a failure.
    #[test]
    fn the_tested_state_round_trips_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_tested(dir.path()), TestedState::default());

        let mut state = TestedState::default();
        state.record(pair("a", "Q4_K_M"));
        save_tested(dir.path(), &state).unwrap();

        assert_eq!(load_tested(dir.path()), state);
    }

    /// **The load-smoke gate deletes what will not load.** A candidate that
    /// fails the smoke is reported unloadable and gone, never benched.
    #[test]
    fn the_smoke_gate_deletes_an_unloadable_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bad.gguf");
        std::fs::write(&file, b"weights").unwrap();

        let outcome = smoke_gate(&file, || Err("the model would not load".to_owned()));

        assert_eq!(outcome, Err("the model would not load".to_owned()));
        assert!(!file.exists(), "an unloadable candidate is deleted");
    }

    /// A candidate that passes the smoke keeps its file for the bench.
    #[test]
    fn the_smoke_gate_keeps_a_loadable_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("good.gguf");
        std::fs::write(&file, b"weights").unwrap();

        smoke_gate(&file, || Ok(())).unwrap();

        assert!(file.exists(), "a loadable candidate stays for the bench");
    }

    /// **A direct hit is unverified and carries its own resolve URL.** The
    /// report marks it direct so the feed stays the recommendation source.
    #[test]
    fn a_direct_hit_is_marked_unverified_and_names_its_url() {
        let siblings = serde_json::json!([
            { "rfilename": "model.Q4_K_M.gguf", "size": 100, "lfs": { "sha256": "ab" } },
            { "rfilename": "README.md", "size": 10 },
        ]);

        let hits = direct_hits("owner/model", Some(42), &siblings);
        let candidate = hits[0].candidate();

        assert_eq!(candidate.source, CandidateSource::Direct);
        assert_eq!(candidate.variant, "owner/model");
        assert_eq!(candidate.quant, "Q4_K_M");
        assert_eq!(candidate.size, 100);
        assert_eq!(candidate.sha256.as_deref(), Some("ab"));
        assert_eq!(
            candidate.url.as_deref(),
            Some("https://huggingface.co/owner/model/resolve/main/model.Q4_K_M.gguf")
        );
    }

    /// The direct selection obeys the tested state, the budget and the cap.
    #[test]
    fn the_direct_selection_obeys_the_same_rules() {
        let hits = vec![
            DirectHit {
                repo: "a/model".to_owned(),
                file: "a-Q4_K_M.gguf".to_owned(),
                size: 600,
                sha256: None,
                downloads: Some(100),
                quant: "Q4_K_M".to_owned(),
            },
            DirectHit {
                repo: "b/model".to_owned(),
                file: "b-Q4_K_M.gguf".to_owned(),
                size: 600,
                sha256: None,
                downloads: Some(50),
                quant: "Q4_K_M".to_owned(),
            },
        ];

        let selection = select_direct(&hits, &TestedState::default(), 1_000, 2);

        assert_eq!(selection.candidates.len(), 1);
        assert_eq!(selection.candidates[0].variant, "a/model", "most downloaded first");
        assert!(selection.bytes <= 1_000);
    }

    /// A candidate whose URL is known builds a spec without touching the
    /// network, which is what the download path reads.
    #[test]
    fn a_candidate_with_a_url_builds_its_spec() {
        let candidate = Candidate {
            source: CandidateSource::Feed,
            variant: "a".to_owned(),
            display_name: "a".to_owned(),
            family: "test".to_owned(),
            quant: "Q4_K_M".to_owned(),
            file: "a-Q4_K_M.gguf".to_owned(),
            size: 100,
            sha256: Some("ab".to_owned()),
            url: Some("https://weights.invalid/a-Q4_K_M.gguf".to_owned()),
            published_repo: None,
            speed: None,
            measured_on: None,
            download_count: None,
        };

        let spec = candidate.spec(&CatalogueSource::default()).unwrap();

        assert_eq!(spec.file, "a-Q4_K_M.gguf");
        assert_eq!(spec.size, 100);
        assert_eq!(spec.sha256.as_deref(), Some("ab"));
        assert_eq!(spec.url, "https://weights.invalid/a-Q4_K_M.gguf");
    }
}
