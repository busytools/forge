//! The weekly dictate exploration bench, headless.
//!
//! `#[ignore]`d because it needs the live models on disk, and the full run
//! needs the network and the run's download budget. `just dictate-sweep`
//! runs the full sweep; `just dictate-sweep-dry` refreshes the feed and
//! prints the candidates without downloading; `just dictate-sweep-live`
//! benches the installed live models alone.
//!
//! **It measures the models the live roles resolve to on this machine**,
//! off a copy of the store the running server holds. It never writes that
//! store: its results go to a JSON file under the bench cache, and its
//! candidates are downloaded there, never into the models directory.
//!
//! **One table per model**: the tier, its clips, the word error rate where
//! the words are known, the mean divergence on the saved takes, the
//! realtime factor, and the takes where a candidate's words differ from
//! the in-use model's, with both texts.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use forge_dictate::Config;
use forge_dictate::bench::{ClipRun, Corpus, Tier};
use forge_workspace::bench::{LiveRoles, resolved_live_roles};
use forge_workspace::sweep::{
    CandidateSource, Disagreement, DownloadReport, ModelReport, SweepReport, TestedPair,
    TierReport, bench_cache_dir, download_candidate, load_smoke, load_tested, save_tested,
    search_hugging_face, select_candidates, select_direct, smoke_gate, write_report,
};

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`: the config dir a running forge
/// boots from, which is where the roles' resolution lives.
fn config_dir() -> PathBuf {
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = value.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir().expect("a home directory is required to locate the config dir").join(".claude")
}

fn app_support() -> PathBuf {
    forge_sdk::app_support_dir().expect("a data directory is required to locate forge's store")
}

/// The live roles the running forge resolves, read off a store copy.
fn live_roles() -> LiveRoles {
    resolved_live_roles(&config_dir(), &app_support()).unwrap_or_else(|error| {
        panic!("the live dictate roles must resolve before a sweep can measure them: {error}")
    })
}

/// The live pair's config with the bench's own stores off: a bench run must
/// not write takes into the corpus it scores.
fn live_pair_config(live: &LiveRoles) -> Config {
    let mut cfg = live.config.clone();
    cfg.models_dir = Some(live.models_dir.clone());
    cfg.diagnostics_dir = None;
    cfg.digest_cache_dir = None;
    cfg
}

/// The live ASR alone, which is the one-model question's candidate.
fn without_normalizer(live: &LiveRoles) -> Config {
    let mut cfg = live_pair_config(live);
    cfg.normalizer = None;
    cfg
}

/// The config one downloaded candidate runs under. The normalizer is the
/// live one, linked into the bench cache; `with_normalizer` false removes
/// it, which is the one-model run.
fn candidate_config(
    live: &LiveRoles,
    cache: &Path,
    file: &str,
    with_normalizer: bool,
) -> Result<Config, String> {
    let base = live_pair_config(live);
    let target = forge_workspace::bench::BenchTarget {
        file: file.to_owned(),
        role: forge_workspace::bench::BenchRole::Transcribing,
        pinned: false,
    };
    let mut cfg = forge_workspace::bench::config_for(&base, cache, &target)
        .map_err(|error| error.to_string())?;
    cfg.models_dir = Some(cache.to_path_buf());
    if !with_normalizer {
        cfg.normalizer = None;
    }
    Ok(cfg)
}

/// Link the live normalizer into the bench cache, so one engine can load
/// the candidate's ASR and the live normalizer from one directory.
fn link_live_normalizer(live: &LiveRoles, cache: &Path) -> Result<(), String> {
    let Some(normalizer) = &live.config.normalizer else {
        return Ok(());
    };
    let source = live.models_dir.join(&normalizer.file);
    let dest = cache.join(&normalizer.file);
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(cache).map_err(|error| format!("{}: {error}", cache.display()))?;
    std::os::unix::fs::symlink(&source, &dest)
        .map_err(|error| format!("{}: {error}", dest.display()))
}

fn corpus_for(tier: Tier, live: &LiveRoles) -> Result<Corpus, String> {
    let app = app_support();
    let takes =
        live.config.diagnostics_dir.clone().unwrap_or_else(|| app.join("dictate-diagnostics"));
    let read_aloud = app.join("dictate-read-aloud");
    let public = live.models_dir.join("public-corpus");
    forge_dictate::bench::corpus(tier, &takes, &read_aloud, &public)
        .map_err(|error| error.to_string())
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Consensus => "consensus",
        Tier::ReadAloud => "read_aloud",
        Tier::Public => "public",
    }
}

/// Run one config over the three tiers and fold the table.
fn bench_three_tiers(
    cfg: &Config,
    live: &LiveRoles,
    label: String,
    source: &str,
) -> Result<ModelReport, String> {
    let mut tiers = Vec::new();
    let mut disagreements = Vec::new();
    for tier in [Tier::Consensus, Tier::Public, Tier::ReadAloud] {
        let corpus = corpus_for(tier, live)?;
        let (metrics, clips) = forge_dictate::bench::run_with_clips(cfg, &corpus, |_| {
            std::ops::ControlFlow::Continue(())
        })
        .map_err(|error| format!("{label} on {}: {error}", tier_label(tier)))?;
        let (mean_divergence, diffs) = divergence(&clips, &corpus);
        tiers.push(TierReport {
            tier: tier_label(tier).to_owned(),
            clips: metrics.clips,
            wer: metrics.wer,
            mean_divergence,
            xrt_wall: metrics.xrt_wall,
        });
        disagreements.extend(diffs);
    }
    Ok(ModelReport { label, source: source.to_owned(), tiers, disagreements, one_model: None })
}

/// The mean word error rate against the takes' own recorded words, and the
/// takes where the run's words differ, with both texts.
fn divergence(clips: &[ClipRun], corpus: &Corpus) -> (Option<f64>, Vec<Disagreement>) {
    let mut total = 0.0_f64;
    let mut count = 0_u32;
    let mut disagreements = Vec::new();
    for run in clips {
        let Some(clip) = corpus.clips.get(run.index) else {
            continue;
        };
        let Some(recorded) = clip.recorded.as_deref() else {
            continue;
        };
        total += forge_dictate::bench::wer(&run.text, recorded);
        count += 1;
        if run.matched == Some(false) {
            disagreements.push(Disagreement {
                clip: run.index,
                candidate: run.text.clone(),
                in_use: recorded.to_owned(),
            });
        }
    }
    let mean = (count > 0).then(|| total / f64::from(count));
    (mean, disagreements)
}

fn tier_wer(model: &ModelReport, tier: &str) -> Option<f64> {
    model.tiers.iter().find(|row| row.tier == tier).and_then(|row| row.wer)
}

fn tier_xrt(model: &ModelReport, tier: &str) -> f64 {
    model.tiers.iter().find(|row| row.tier == tier).map_or(0.0, |row| row.xrt_wall)
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |rate| format!("{:.2}%", rate * 100.0))
}

/// The one-model question: does the run without the normalizer match or
/// beat the live ASR+normalizer pair on the known words?
fn one_model_answer(alone: &ModelReport, pair: &ModelReport) -> String {
    let public_alone = tier_wer(alone, "public");
    let public_pair = tier_wer(pair, "public");
    let read_alone = tier_wer(alone, "read_aloud");
    let read_pair = tier_wer(pair, "read_aloud");
    let matches_pair = match (public_alone, public_pair) {
        (Some(candidate), Some(live)) => candidate <= live,
        _ => false,
    };
    let verdict = if matches_pair { "matches or beats" } else { "does not match" };
    let public_alone = percent(public_alone);
    let public_pair = percent(public_pair);
    let read_alone = percent(read_alone);
    let read_pair = percent(read_pair);
    let alone_rtf = format!("{:.1}x", tier_xrt(alone, "public"));
    let pair_rtf = format!("{:.1}x", tier_xrt(pair, "public"));
    format!(
        "the model without the normalizer {verdict} the live ASR+normalizer pair on public WER \
         ({public_alone} vs {public_pair}; read-aloud {read_alone} vs {read_pair}); RTF \
         {alone_rtf} vs {pair_rtf}"
    )
}

/// Whether a candidate beats the live pair on the plan's own axis: a lower
/// known-word WER at equal or better realtime factor.
fn beats_live(candidate: &ModelReport, live: &ModelReport) -> bool {
    match (tier_wer(candidate, "public"), tier_wer(live, "public")) {
        (Some(candidate_wer), Some(live_wer)) => {
            candidate_wer < live_wer && tier_xrt(candidate, "public") >= tier_xrt(live, "public")
        }
        _ => false,
    }
}

fn live_files(live: &LiveRoles) -> Vec<String> {
    let mut files = vec![live.config.asr_model.file.clone()];
    if let Some(normalizer) = &live.config.normalizer {
        files.push(normalizer.file.clone());
    }
    files
}

fn sweep_budget() -> u64 {
    std::env::var("DICTATE_SWEEP_BUDGET_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn public_clip(live: &LiveRoles) -> Vec<f32> {
    let public = live.models_dir.join("public-corpus");
    forge_dictate::bench::public_corpus(&public)
        .expect("the installed public slice must read")
        .into_iter()
        .next()
        .map(|clip| clip.audio)
        .unwrap_or_default()
}

fn print_model(model: &ModelReport) {
    let label = &model.label;
    let source = &model.source;
    println!("\n--- {label} ({source}) ---");
    println!("{:<12} {:>5} {:>9} {:>13} {:>8}", "tier", "clips", "WER", "divergence", "RTF");
    for row in &model.tiers {
        let tier = &row.tier;
        let clips = row.clips;
        let wer = percent(row.wer);
        let divergence = percent(row.mean_divergence);
        let xrt = row.xrt_wall;
        println!("{tier:<12} {clips:>5} {wer:>9} {divergence:>13} {xrt:>8.1}x");
    }
    if let Some(answer) = &model.one_model {
        println!("one-model: {answer}");
    }
    if model.disagreements.is_empty() {
        return;
    }
    println!("takes where the words differ:");
    for row in &model.disagreements {
        let clip = row.clip;
        let candidate = &row.candidate;
        let in_use = &row.in_use;
        println!("  clip {clip}: candidate {candidate:?} vs in-use {in_use:?}");
    }
}

fn print_report(report: &SweepReport) {
    let at = &report.at;
    println!("\n=== dictate sweep {at} ===");
    if let Some(release) = &report.feed_release {
        let entries = report.models.len();
        println!("feed release {release}; {entries} models measured");
    }
    for model in &report.models {
        print_model(model);
    }
    if report.downloaded.is_empty() {
        println!("\nno candidate was downloaded");
    } else {
        println!("\ndownloads:");
        for row in &report.downloaded {
            let variant = &row.variant;
            let quant = &row.quant;
            let file = &row.file;
            let outcome = &row.outcome;
            let reason = row.reason.as_deref().unwrap_or("");
            println!("  {variant} {quant} -> {file}: {outcome} {reason}");
        }
    }
    if !report.proposed.is_empty() {
        println!("\nproposals (nothing switched):");
        for proposal in &report.proposed {
            println!("  {proposal}");
        }
    }
}

/// One direct hit's candidate is unverified; the report says so and the feed
/// stays the recommendation source.
fn direct_selection(
    catalogue: &forge_dictate::catalogue::Catalogue,
    tested: &forge_workspace::sweep::TestedState,
    budget: u64,
) -> forge_workspace::sweep::Selection {
    let mut families: Vec<String> = Vec::new();
    for entry in &catalogue.entries {
        if !entry.family.is_empty() && !families.contains(&entry.family) {
            families.push(entry.family.clone());
        }
    }
    let hits = search_hugging_face(&families, 5).unwrap_or_default();
    select_direct(&hits, tested, budget, 2)
}

/// The real sweep over the installed live models: the pair and the ASR
/// alone over the saved takes and the known-word tiers, with no feed and no
/// download.
#[test]
#[ignore = "needs the live models on disk; run via just dictate-sweep-live"]
fn dictate_sweep_live() {
    let live = live_roles();
    let app = app_support();
    let cache = bench_cache_dir(&app);
    forge_dictate::bench::install_public_corpus(&live.models_dir)
        .expect("the bundled public slice installs");

    let pair = bench_three_tiers(
        &live_pair_config(&live),
        &live,
        "live ASR+normalizer".to_owned(),
        "live",
    )
    .expect("the live pair must load");
    let mut alone = bench_three_tiers(
        &without_normalizer(&live),
        &live,
        "live ASR (no normalizer)".to_owned(),
        "live",
    )
    .expect("the live ASR must load");
    alone.one_model = Some(one_model_answer(&alone, &pair));

    let report = SweepReport {
        at: now(),
        feed_release: None,
        feed_skipped: 0,
        budget_bytes: 0,
        downloaded: Vec::new(),
        models: vec![pair, alone],
        proposed: Vec::new(),
    };
    print_report(&report);
    write_report(&cache, &report).expect("the report must write");
    let path = cache.join(forge_workspace::sweep::RESULTS_FILE);
    println!("\nwrote {}", path.display());
}

/// The discovery dry run: refresh the feed, diff it against the tested
/// state, and print the candidates. Nothing is downloaded.
#[test]
#[ignore = "needs the network; run via just dictate-sweep-dry"]
fn dictate_sweep_discovery() {
    let live = live_roles();
    let app = app_support();
    let cache = bench_cache_dir(&app);
    let source = forge_dictate::catalogue::CatalogueSource::default();
    let catalogue =
        forge_dictate::catalogue::fetch_catalogue(&source).expect("the feed must refresh");
    forge_dictate::catalogue::write_catalogue_cache(&app.join("dictate-catalogue"), &catalogue)
        .expect("the feed must cache");
    let tested = load_tested(&cache);
    let budget = sweep_budget();
    let selection = select_candidates(&catalogue.entries, &tested, &live_files(&live), budget, 2);

    println!("\n=== dictate sweep discovery (no download) ===");
    let release = catalogue.release.as_deref().unwrap_or("unknown");
    let entries = catalogue.entries.len();
    let unreadable = catalogue.skipped;
    let tested_count = tested.pairs.len();
    println!("feed release {release}: {entries} entries, {unreadable} unreadable documents");
    println!("tested state: {tested_count} pairs");
    let selected = selection.candidates.len();
    let bytes = selection.bytes;
    println!("budget: {budget} bytes; selected {selected} candidates for {bytes} bytes");
    for candidate in &selection.candidates {
        let variant = &candidate.variant;
        let quant = &candidate.quant;
        let size = candidate.size;
        let speed = candidate.speed.unwrap_or(0.0);
        let measured = candidate.measured_on.as_deref().unwrap_or("undated");
        println!("  {variant} {quant}: {size} bytes, {speed:.1}x, measured {measured}");
    }
    if !selection.skipped.is_empty() {
        println!("not taken:");
        for skip in &selection.skipped {
            let variant = &skip.variant;
            let quant = &skip.quant;
            let reason = &skip.reason;
            println!("  {variant} {quant}: {reason}");
        }
    }
    if std::env::var("DICTATE_SWEEP_DIRECT").is_ok() {
        let direct = direct_selection(&catalogue, &tested, budget);
        let hits = direct.candidates.len();
        println!("direct hits: {hits} (unverified, each must pass the load smoke)");
        for candidate in &direct.candidates {
            let variant = &candidate.variant;
            let quant = &candidate.quant;
            let size = candidate.size;
            println!("  {variant} {quant}: {size} bytes");
        }
    }
}

/// The weekly run: refresh the feed, take at most two candidates within the
/// budget, download them into the bench cache, smoke each one, bench what
/// loads over the takes and the known-word tiers, and delete what did not
/// beat the live pair.
#[test]
#[ignore = "needs the live models, the network and the run's budget; run via just dictate-sweep"]
fn dictate_sweep() {
    let live = live_roles();
    let app = app_support();
    let cache = bench_cache_dir(&app);
    forge_dictate::bench::install_public_corpus(&live.models_dir)
        .expect("the bundled public slice installs");

    let source = forge_dictate::catalogue::CatalogueSource::default();
    let catalogue =
        forge_dictate::catalogue::fetch_catalogue(&source).expect("the feed must refresh");
    forge_dictate::catalogue::write_catalogue_cache(&app.join("dictate-catalogue"), &catalogue)
        .expect("the feed must cache");
    let mut tested = load_tested(&cache);
    let budget = sweep_budget();
    let mut selection =
        select_candidates(&catalogue.entries, &tested, &live_files(&live), budget, 2);
    // The feed is the recommendation source; the direct Hub search is the
    // fallback for a week the feed has nothing new. Every direct hit must
    // pass the same load smoke before its bench.
    if selection.candidates.is_empty() && std::env::var("DICTATE_SWEEP_DIRECT").is_ok() {
        selection = direct_selection(&catalogue, &tested, budget);
    }

    let pair = bench_three_tiers(
        &live_pair_config(&live),
        &live,
        "live ASR+normalizer".to_owned(),
        "live",
    )
    .expect("the live pair must load");
    let mut alone = bench_three_tiers(
        &without_normalizer(&live),
        &live,
        "live ASR (no normalizer)".to_owned(),
        "live",
    )
    .expect("the live ASR must load");
    alone.one_model = Some(one_model_answer(&alone, &pair));
    let live_pair = pair.clone();

    let mut report = SweepReport {
        at: now(),
        feed_release: catalogue.release.clone(),
        feed_skipped: catalogue.skipped,
        budget_bytes: budget,
        downloaded: Vec::new(),
        models: vec![pair, alone],
        proposed: Vec::new(),
    };

    let clip = public_clip(&live);
    link_live_normalizer(&live, &cache)
        .expect("the live normalizer must link into the bench cache");

    for candidate in &selection.candidates {
        let unloadable = |report: &mut SweepReport,
                          tested: &mut forge_workspace::sweep::TestedState,
                          reason: String| {
            report.downloaded.push(DownloadReport {
                variant: candidate.variant.clone(),
                quant: candidate.quant.clone(),
                file: candidate.file.clone(),
                size: candidate.size,
                sha256: candidate.sha256.clone(),
                outcome: "unloadable".to_owned(),
                reason: Some(reason),
            });
            tested.record(TestedPair {
                variant: candidate.variant.clone(),
                quant: candidate.quant.clone(),
                at: now(),
                outcome: "unloadable".to_owned(),
            });
        };

        let spec = match candidate.spec(&source) {
            Ok(spec) => spec,
            Err(reason) => {
                unloadable(&mut report, &mut tested, reason);
                continue;
            }
        };
        let path = match download_candidate(candidate, &spec, &cache) {
            Ok(path) => path,
            Err(reason) => {
                unloadable(&mut report, &mut tested, reason);
                continue;
            }
        };
        let with_normalizer = match candidate_config(&live, &cache, &candidate.file, true) {
            Ok(cfg) => cfg,
            Err(reason) => {
                unloadable(&mut report, &mut tested, reason);
                continue;
            }
        };
        let no_normalizer = match candidate_config(&live, &cache, &candidate.file, false) {
            Ok(cfg) => cfg,
            Err(reason) => {
                unloadable(&mut report, &mut tested, reason);
                continue;
            }
        };
        if let Err(reason) = smoke_gate(&path, || load_smoke(&no_normalizer, &clip)) {
            unloadable(&mut report, &mut tested, reason);
            continue;
        }

        let file = &candidate.file;
        let source_label = match candidate.source {
            CandidateSource::Feed => "feed",
            CandidateSource::Direct => "direct (unverified)",
        };
        let with = match bench_three_tiers(
            &with_normalizer,
            &live,
            format!("{file} + normalizer"),
            source_label,
        ) {
            Ok(model) => model,
            Err(reason) => {
                unloadable(&mut report, &mut tested, reason);
                continue;
            }
        };
        let mut without = match bench_three_tiers(
            &no_normalizer,
            &live,
            format!("{file} (no normalizer)"),
            source_label,
        ) {
            Ok(model) => model,
            Err(reason) => {
                unloadable(&mut report, &mut tested, reason);
                continue;
            }
        };
        let answer = one_model_answer(&without, &live_pair);
        without.one_model = Some(answer.clone());

        let beats = beats_live(&with, &live_pair);
        report.downloaded.push(DownloadReport {
            variant: candidate.variant.clone(),
            quant: candidate.quant.clone(),
            file: candidate.file.clone(),
            size: candidate.size,
            sha256: candidate.sha256.clone(),
            outcome: "benched".to_owned(),
            reason: None,
        });
        tested.record(TestedPair {
            variant: candidate.variant.clone(),
            quant: candidate.quant.clone(),
            at: now(),
            outcome: "benched".to_owned(),
        });
        let candidate_wer = percent(tier_wer(&with, "public"));
        let live_wer = percent(tier_wer(&live_pair, "public"));
        let candidate_xrt = tier_xrt(&with, "public");
        let live_xrt = tier_xrt(&live_pair, "public");
        report.models.push(with);
        report.models.push(without);
        if beats {
            let variant = &candidate.variant;
            let quant = &candidate.quant;
            report.proposed.push(format!(
                "{variant} {quant}: public WER {candidate_wer} vs {live_wer}; RTF \
                 {candidate_xrt:.1}x vs {live_xrt:.1}x; {answer}"
            ));
        } else if let Err(error) = std::fs::remove_file(&path) {
            println!("  {file} was not removed: {error}");
        }
    }

    print_report(&report);
    save_tested(&cache, &tested).expect("the tested state must write");
    write_report(&cache, &report).expect("the report must write");
    let path = cache.join(forge_workspace::sweep::RESULTS_FILE);
    println!("\nwrote {}", path.display());
}
