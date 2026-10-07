//! The transcribe.cpp catalogue: the per-variant feed the runtime
//! publishes.
//!
//! Each variant is one JSON document (`catalog/<variant>.json`) carrying
//! the model's identity, its downloads, and the measured accuracy and
//! speed rows. Parsing is tolerant by design: upstream grows the schema,
//! so unknown fields are ignored and the optional blocks default, while a
//! document from another schema is refused by name rather than
//! half-read.

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::Error;

/// The schema string every entry this build reads carries.
pub const SCHEMA: &str = "transcribe-catalog-v1";

/// The quants a headline accuracy figure is read at, most preferred
/// first: the reference quants the catalogue measures its headline rows
/// at, so a comparison across variants reads the same axis.
const REFERENCE_QUANTS: [&str; 3] = ["Q8_0", "F16", "BF16"];

/// One variant's catalogue entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogueEntry {
    pub variant: String,
    /// Missing falls back to the variant itself.
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub family: String,
    /// Parameter count of the upstream checkpoint.
    #[serde(default)]
    pub params: u64,
    #[serde(default)]
    pub license: Option<License>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub headline_benchmark: Option<HeadlineBenchmark>,
    /// The feed's published Hugging Face repo for this variant
    /// (`owner/name`), which is where its README's own download table lives
    /// when the docs tree has no per-variant page.
    #[serde(default)]
    pub published_repo: Option<String>,
    #[serde(default)]
    pub downloads: Vec<Download>,
    #[serde(default)]
    pub speed_benchmarks: Vec<SpeedRow>,
    #[serde(default)]
    pub accuracy_benchmarks: Vec<AccuracyRow>,
    /// What this entry is for. The speech feed's documents never spell it -
    /// they are all speech models - and the Hub's feed sets it for every
    /// entry it builds.
    #[serde(default)]
    pub kind: EntryKind,
    /// How many times the Hub has served the repo. **The only pre-run signal
    /// a cleanup candidate has** - the feed publishes no speed and no error
    /// for a normalizer - so it is what orders a sweep. The speech feed
    /// carries no count and leaves this `None`.
    #[serde(default)]
    pub download_count: Option<u64>,
}

impl CatalogueEntry {
    /// Whether the variant supports streaming recognition.
    pub fn streaming(&self) -> bool {
        self.capabilities.streaming.supported
    }

    /// The download carrying this exact file name, which is how a pinned
    /// model is joined to its entry.
    pub fn download_for(&self, filename: &str) -> Option<&Download> {
        self.downloads.iter().find(|download| download.filename == filename)
    }

    /// The m4-max Metal row carrying the best wall-clock realtime factor,
    /// so the machine, backend and quant cross beside the number.
    pub fn m4_metal_speed_row(&self) -> Option<&SpeedRow> {
        self.speed_benchmarks
            .iter()
            .filter(|row| {
                row.machine == "m4-max" && row.backend == "metal" && row.xrt_wall.is_some()
            })
            .max_by(|a, b| {
                a.xrt_wall.unwrap_or_default().total_cmp(&b.xrt_wall.unwrap_or_default())
            })
    }

    /// The best m4-max Metal wall-clock realtime factor across the
    /// quantised speed rows. `None` when the feed carries no such row.
    pub fn m4_metal_xrt_wall(&self) -> Option<f64> {
        self.m4_metal_speed_row().and_then(|row| row.xrt_wall)
    }

    /// Word error rate on FLEURS English at the reference quant.
    pub fn fleurs_en_wer(&self) -> Option<f64> {
        self.wer_for("fleurs", "test", "en")
    }

    /// Word error rate on the entry's own headline benchmark.
    pub fn headline_wer(&self) -> Option<f64> {
        let headline = self.headline_benchmark.as_ref()?;
        self.wer_for(&headline.dataset, &headline.split, &headline.language)
    }

    fn wer_for(&self, dataset: &str, split: &str, language: &str) -> Option<f64> {
        REFERENCE_QUANTS.iter().find_map(|quant| {
            self.accuracy_benchmarks
                .iter()
                .find(|row| {
                    row.dataset == dataset
                        && row.split == split
                        && row.language == language
                        && &row.quant == quant
                })
                .and_then(|row| row.err_pct)
        })
    }
}

/// The licence block, in the two forms the feed carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct License {
    #[serde(default)]
    pub spdx: String,
    #[serde(default)]
    pub display: String,
}

/// What a variant can do, as far as the feed verifies it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub streaming: Support,
}

/// One capability's support flag.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Support {
    #[serde(default)]
    pub supported: bool,
}

/// The benchmark a variant's headline figure comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadlineBenchmark {
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub split: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub metric: String,
}

/// What one catalogue entry is FOR: the slot it can fill. **Exhaustive on
/// purpose** - a consumer that ignores it does not compile, so the two feeds'
/// entries can never be mixed up by an assumption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A speech model: what transcribes.
    #[default]
    Asr,
    /// A normalizer: what cleans a transcript up. The Hub's feed.
    Normalizer,
}

/// One downloadable quantisation of the variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Download {
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub size_bytes: u64,
    /// The file's own digest, where its host publishes one: the Hub's blobs
    /// carry a sha256 per file, and the speech feed's documents carry none -
    /// which is the difference the install records as a fact.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The file itself, when the feed names it directly rather than through
    /// a document's link table. The Hub's blobs carry no URL, so this is the
    /// resolve path its own convention spells; a speech feed's row leaves
    /// this `None` and the install reads the doc's table by file name.
    #[serde(default)]
    pub url: Option<String>,
}

/// One measured speed row. Many fields cross that nothing reads; the
/// wall-clock realtime factor is the one the page compares on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeedRow {
    #[serde(default)]
    pub machine: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub xrt_wall: Option<f64>,
}

/// One measured accuracy row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccuracyRow {
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub split: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub err_pct: Option<f64>,
}

/// Parse one catalogue document, refusing anything that is not an entry
/// of the schema this build reads.
pub fn parse_entry(raw: &str) -> Result<CatalogueEntry, Error> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|source| {
        Error::Catalogue { message: format!("the document is not JSON: {source}") }
    })?;
    match value.get("schema").and_then(serde_json::Value::as_str) {
        Some(SCHEMA) => {}
        Some(other) => {
            return Err(Error::Catalogue {
                message: format!(
                    "the document's schema is {other:?}, and this build reads {SCHEMA:?}"
                ),
            });
        }
        None => {
            return Err(Error::Catalogue {
                message: format!("the document carries no {SCHEMA:?} schema"),
            });
        }
    }
    let mut entry: CatalogueEntry = serde_json::from_value(value).map_err(|source| {
        Error::Catalogue { message: format!("the entry does not parse: {source}") }
    })?;
    if entry.display_name.is_empty() {
        entry.display_name.clone_from(&entry.variant);
    }
    Ok(entry)
}

/// Bytes any single catalogue response may carry before the fetch
/// refuses to buffer more. The feed's documents are about 13 KiB; the
/// cap is what keeps a broken mirror from growing this process.
pub(crate) const MAX_RESPONSE_BYTES: u64 = 4 << 20;

/// How many entry documents are fetched at once. The feed is roughly 75
/// small files, so this is what turns a ten-second serial walk into one
/// that finishes while the page is still drawing its first frame.
const FETCH_WORKERS: usize = 6;

/// Where the feed is enumerated, fetched and versioned.
#[derive(Debug, Clone)]
pub struct CatalogueSource {
    /// The endpoint listing the catalogue directory's files.
    pub listing: String,
    /// URL prefix an entry's own file name is appended to.
    pub entry_base: String,
    /// The project's latest release, whose tag labels the feed.
    pub release: String,
    /// URL prefix a variant's per-model doc is appended to, which is where
    /// the download links for its files live.
    pub doc_base: String,
    /// Where the feed's published model repos live, for the README that
    /// carries a variant's own download table when the docs tree has no
    /// per-variant page: `<repo_base><published_repo>/raw/main/README.md`.
    pub repo_base: String,
}

impl Default for CatalogueSource {
    fn default() -> Self {
        Self {
            listing: "https://api.github.com/repos/handy-computer/transcribe.cpp/contents/catalog"
                .to_owned(),
            entry_base:
                "https://raw.githubusercontent.com/handy-computer/transcribe.cpp/main/catalog/"
                    .to_owned(),
            release: "https://api.github.com/repos/handy-computer/transcribe.cpp/releases/latest"
                .to_owned(),
            doc_base:
                "https://raw.githubusercontent.com/handy-computer/transcribe.cpp/main/docs/models/"
                    .to_owned(),
            repo_base: "https://huggingface.co/".to_owned(),
        }
    }
}

/// The markers the feed wraps its machine-readable regions in.
const DOWNLOADS_OPEN: &str = "<!-- catalog:downloads -->";
const TABLE_CLOSE: &str = "<!-- /catalog -->";

/// The download links one variant's doc carries, by file name.
///
/// **Read from the doc's marked region, never the whole prose.** A doc's
/// intro may link the upstream repository holding the ORIGINAL weights,
/// which is not the GGUF this runtime loads, so taking any URL on the page
/// would offer an install that cannot work. A file name is the URL's own
/// last segment, so the name a download is recorded under and the URL it
/// came from cannot disagree.
///
/// A doc this build does not understand yields nothing rather than a guess,
/// and the caller reports that the doc carried no table.
pub fn doc_links(raw: &str) -> Vec<(String, String)> {
    let Some(region) = raw
        .split_once(DOWNLOADS_OPEN)
        .and_then(|(_, rest)| rest.split_once(TABLE_CLOSE))
        .map(|(table, _)| table)
    else {
        return Vec::new();
    };
    gguf_links(region)
}

/// Every `.gguf` link in a whole document, by file name.
///
/// The published repo's README is not a generated page and carries no
/// marked region - its table IS the page - so this is the scan for it,
/// with the same file-name rule [`doc_links`] uses.
pub fn file_links(raw: &str) -> Vec<(String, String)> {
    gguf_links(raw)
}

fn gguf_links(region: &str) -> Vec<(String, String)> {
    let mut links = Vec::new();
    for target in region.lines().flat_map(link_targets) {
        let Some((_, file)) = target.rsplit_once('/') else {
            continue;
        };
        let is_gguf = std::path::Path::new(file)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"));
        if !is_gguf {
            continue;
        }
        links.push((file.to_owned(), target));
    }
    links
}

/// Every markdown link target on one line: the text between `](` and `)`.
fn link_targets(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        targets.push(after[..end].to_owned());
        rest = &after[end..];
    }
    targets
}

/// The feed as one fetch found it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalogue {
    /// RFC 3339, stamped when the fetch ran.
    pub fetched_at: String,
    /// The upstream release tag the feed stood at, when it answered.
    pub release: Option<String>,
    pub entries: Vec<CatalogueEntry>,
    /// How many listed documents did not become entries: a bad document
    /// never fails the feed, and this is what keeps the loss visible.
    pub skipped: usize,
}

/// Fetch the whole catalogue. Blocking, like everything else here.
///
/// The listing is the spine: without it this refuses. A single document
/// that cannot be read is skipped and counted, and a feed whose every
/// document failed is refused rather than answered empty - a page drawn
/// over an empty catalogue reads the same as a page over a healthy one.
pub fn fetch_catalogue(source: &CatalogueSource) -> Result<Catalogue, Error> {
    let client = feed_client(&source.listing)?;
    let listing = get_bounded_text(&client, &source.listing, MAX_RESPONSE_BYTES)?;
    let names = listed_files(&listing);
    if names.is_empty() {
        return Err(Error::Catalogue {
            message: format!("the listing at {} named no entry documents", source.listing),
        });
    }

    let chunk_size = names.len().div_ceil(FETCH_WORKERS).max(1);
    let (entries, skipped) = std::thread::scope(|scope| {
        let workers: Vec<_> = names
            .chunks(chunk_size)
            .map(|chunk| {
                let client = &client;
                let base = source.entry_base.as_str();
                scope.spawn(move || fetch_chunk(client, base, chunk))
            })
            .collect();
        let mut entries = Vec::new();
        let mut skipped = 0_usize;
        for worker in workers {
            let (chunk_entries, chunk_skipped) =
                worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            entries.extend(chunk_entries);
            skipped += chunk_skipped;
        }
        (entries, skipped)
    });

    if entries.is_empty() {
        return Err(Error::Catalogue {
            message: format!(
                "none of the {} catalogue entries listed under {} could be read",
                names.len(),
                source.entry_base
            ),
        });
    }

    // The release is a label, not the feed: an answer this build cannot
    // read costs the tag and nothing else.
    let release = get_bounded_text(&client, &source.release, MAX_RESPONSE_BYTES)
        .ok()
        .and_then(|raw| release_tag(&raw));

    Ok(Catalogue { fetched_at: rfc3339_now(), release, entries, skipped })
}

/// The entry documents a listing names: JSON files, never the
/// underscore-prefixed schema and profile records beside them.
fn listed_files(listing: &str) -> Vec<String> {
    let Ok(files) = serde_json::from_str::<Vec<serde_json::Value>>(listing) else {
        return Vec::new();
    };
    files
        .iter()
        .filter(|file| file.get("type").and_then(serde_json::Value::as_str) == Some("file"))
        .filter_map(|file| file.get("name").and_then(serde_json::Value::as_str))
        .filter(|name| {
            Path::new(name).extension().is_some_and(|ext| ext == "json") && !name.starts_with('_')
        })
        .map(str::to_owned)
        .collect()
}

/// Fetch and parse one slice of the listing; a document that cannot be
/// read is counted, not returned.
fn fetch_chunk(
    client: &reqwest::blocking::Client,
    base: &str,
    names: &[String],
) -> (Vec<CatalogueEntry>, usize) {
    let mut entries = Vec::new();
    let mut skipped = 0_usize;
    for name in names {
        let url = format!("{base}{name}");
        match get_bounded_text(client, &url, MAX_RESPONSE_BYTES).and_then(|raw| parse_entry(&raw)) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                skipped += 1;
                tracing::debug!(
                    event_name = "dictate_catalogue_entry_skipped",
                    %url,
                    %error,
                    "a catalogue entry could not be read; the rest of the feed stands"
                );
            }
        }
    }
    (entries, skipped)
}

/// Fetch one variant's per-model doc, where its own download links live.
///
/// Blocking, like the feed's fetch, and worth calling only when a model is
/// being installed or a config key names a variant that is not on disk.
pub fn fetch_doc(source: &CatalogueSource, variant: &str) -> Result<String, Error> {
    let client = feed_client(&source.doc_base)?;
    let url = format!("{}{variant}.md", source.doc_base);
    get_bounded_text(&client, &url, MAX_RESPONSE_BYTES)
}

/// The download links a variant's own documents carry, by file name.
///
/// **Two documents can carry them and either may answer.** A variant whose
/// docs-tree page exists has its table there, read through the marked
/// region; the language-specific fine-tunes - moonshine's Arabic and
/// Japanese builds, breeze - have no page in the tree at all, and keep
/// their table in the published repo's README instead. The doc is read
/// first; a doc that is absent, or carries no table, falls through to the
/// README. Both are parsed for their links, never constructed from the
/// file name.
///
/// Blocking, like the feed's fetch, and worth calling only when a model is
/// being installed or a config key names one that is not on disk.
pub fn download_links(
    source: &CatalogueSource,
    variant: &str,
    published_repo: Option<&str>,
) -> Result<Vec<(String, String)>, Error> {
    let mut tried = Vec::new();

    let doc_url = format!("{}{variant}.md", source.doc_base);
    match fetch_doc(source, variant) {
        Ok(raw) => {
            let links = doc_links(&raw);
            if !links.is_empty() {
                return Ok(links);
            }
            tried.push(format!("{doc_url} carries no download table"));
        }
        Err(error) => tried.push(error.to_string()),
    }

    if let Some(repo) = published_repo {
        let readme_url = format!("{}{repo}/raw/main/README.md", source.repo_base);
        let client = feed_client(&source.repo_base)?;
        match get_bounded_text(&client, &readme_url, MAX_RESPONSE_BYTES) {
            Ok(raw) => {
                let links = file_links(&raw);
                if !links.is_empty() {
                    return Ok(links);
                }
                tried.push(format!("{readme_url} carries no .gguf link"));
            }
            Err(error) => tried.push(error.to_string()),
        }
    }

    Err(Error::Catalogue {
        message: format!("no download links for {variant}: {}", tried.join("; ")),
    })
}

/// The client every feed request goes through: the timeouts, and the
/// User-Agent api.github.com refuses a request without.
pub(crate) fn feed_client(for_url: &str) -> Result<reqwest::blocking::Client, Error> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("forge-dictate/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| Error::Http { url: for_url.to_owned(), source: error })
}

/// GET one response, bounded: a status that is not success is its own
/// error, and a body past `cap` is refused rather than buffered.
pub(crate) fn get_bounded_text(
    client: &reqwest::blocking::Client,
    url: &str,
    cap: u64,
) -> Result<String, Error> {
    let response =
        client.get(url).send().map_err(|source| Error::Http { url: url.to_owned(), source })?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus { url: url.to_owned(), status: status.as_u16() });
    }
    let mut body = String::new();
    response.take(cap + 1).read_to_string(&mut body).map_err(|source| Error::Catalogue {
        message: format!("{url} could not be read: {source}"),
    })?;
    if body.len() as u64 > cap {
        return Err(Error::Catalogue {
            message: format!("{url} answered more than {cap} bytes; refused unread"),
        });
    }
    Ok(body)
}

/// The tag of a GitHub release document.
fn release_tag(raw: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()?
        .get("tag_name")?
        .as_str()
        .map(str::to_owned)
}

/// Now, in RFC 3339 UTC. A clock before the epoch formats as the epoch
/// rather than failing: a wrong label beats no feed.
fn rfc3339_now() -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    time::OffsetDateTime::from_unix_timestamp(i64::try_from(now.as_secs()).unwrap_or(0))
        .ok()
        .and_then(|at| at.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

/// The record of the last fetched feed under [`read_catalogue_cache`]'s
/// directory.
const CATALOGUE_CACHE_FILE: &str = "catalogue.json";

/// The record's own layout version. A file written by a layout this
/// build does not know is ignored rather than misread.
///
/// **Bump this whenever the entry shape grows a field the download path
/// reads.** Every entry field defaults, so a cache written by an older
/// build parses cleanly and answers the default - and a default is a
/// silent wrong answer where a refetch is the right one. Bumped to 2 for
/// `published_repo`: a cache written without it cannot find the download
/// links of the variants whose only document is their repo's README, and
/// the install failed a 404 on a doc that never existed.
const CATALOGUE_CACHE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct CatalogueFile {
    version: u32,
    fetched_at: String,
    release: Option<String>,
    entries: Vec<CatalogueEntry>,
    skipped: usize,
}

/// Read the last fetched catalogue under `dir`, when there is one this
/// build wrote.
pub fn read_catalogue_cache(dir: &Path) -> Result<Option<Catalogue>, Error> {
    let file = dir.join(CATALOGUE_CACHE_FILE);
    let raw = match std::fs::read_to_string(&file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Io { path: file, source }),
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| Error::Catalogue {
            message: format!("{} is not a catalogue cache: {source}", file.display()),
        })?;
    // The version is read before the shape: a file another layout wrote
    // is no catalogue, not a corrupt one, and its fields are not this
    // build's to require.
    if value.get("version").and_then(serde_json::Value::as_u64)
        != Some(u64::from(CATALOGUE_CACHE_VERSION))
    {
        return Ok(None);
    }
    let parsed: CatalogueFile =
        serde_json::from_value(value).map_err(|source| Error::Catalogue {
            message: format!("{} is not a catalogue cache: {source}", file.display()),
        })?;
    Ok(Some(Catalogue {
        fetched_at: parsed.fetched_at,
        release: parsed.release,
        entries: parsed.entries,
        skipped: parsed.skipped,
    }))
}

/// Write the feed under `dir`, so a page reads the last-known rows
/// without the network.
pub fn write_catalogue_cache(dir: &Path, catalogue: &Catalogue) -> Result<(), Error> {
    let body = serde_json::to_string(&CatalogueFile {
        version: CATALOGUE_CACHE_VERSION,
        fetched_at: catalogue.fetched_at.clone(),
        release: catalogue.release.clone(),
        entries: catalogue.entries.clone(),
        skipped: catalogue.skipped,
    })
    .map_err(|source| Error::Catalogue {
        message: format!("the catalogue did not serialise: {source}"),
    })?;
    std::fs::create_dir_all(dir).map_err(|source| Error::Io { path: dir.to_path_buf(), source })?;
    // Written beside and renamed over, like the model files: a process
    // that dies mid-write leaves the previous cache standing rather
    // than a truncated one the offline read would find.
    let file = dir.join(CATALOGUE_CACHE_FILE);
    let partial = dir.join(format!("{CATALOGUE_CACHE_FILE}.tmp"));
    std::fs::write(&partial, body).map_err(|source| Error::Io { path: partial.clone(), source })?;
    std::fs::rename(&partial, &file).map_err(|source| Error::Io { path: file, source })
}

#[cfg(test)]
mod tests_catalogue {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/fixtures/catalogue/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).expect("the fixture must be readable")
    }

    /// The identity facts the page's IN USE and FIND A MODEL rows draw.
    #[test]
    fn a_real_entry_parses_into_the_identity_the_page_draws() {
        let entry = parse_entry(&fixture("cohere-transcribe-03-2026.json"))
            .expect("the published entry must parse");

        assert_eq!(entry.variant, "cohere-transcribe-03-2026");
        assert_eq!(entry.params, 2_049_026_832, "the parameter count is the feed's own");
        assert_eq!(
            entry.license.as_ref().map(|l| l.display.as_str()),
            Some("Apache-2.0"),
            "the licence crosses in the form the feed displays"
        );
        assert_eq!(entry.languages.len(), 14, "every language the feed lists is carried");
        assert!(entry.languages.iter().any(|l| l == "en"));
        assert!(!entry.streaming(), "cohere-transcribe is not a streaming variant");
    }

    /// The pinned file is joined to the catalogue by name and size: this is
    /// what makes an update check possible at all.
    #[test]
    fn the_pinned_file_is_found_among_the_downloads() {
        let entry = parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse");

        let download = entry
            .download_for("cohere-transcribe-03-2026-Q4_K_M.gguf")
            .expect("the pinned quant must be carried");
        assert_eq!(download.quant, "Q4_K_M");
        assert_eq!(
            download.size_bytes, 1_558_162_944,
            "the size is the witness the pin is joined on"
        );

        assert!(
            entry.download_for("nothing-ships-this-name.gguf").is_none(),
            "a name the feed does not carry must answer None, not a guess"
        );
    }

    /// A document from a schema this build does not know is refused by
    /// name: half-reading a future entry is how a page draws wrong facts.
    #[test]
    fn an_entry_from_another_schema_is_refused_by_name() {
        let err = parse_entry(r#"{"schema": "transcribe-catalog-v2", "variant": "x"}"#)
            .expect_err("a foreign schema must be refused");
        assert!(
            err.to_string().contains("transcribe-catalog-v2"),
            "the refusal must name the schema it saw, got: {err}"
        );
    }

    /// Anything that is not an entry at all is a refusal, never a
    /// half-defaulted row on a page. Each refusal names what it saw.
    #[test]
    fn a_document_that_is_not_an_entry_is_refused_rather_than_half_read() {
        let err =
            parse_entry(r#"{"hello": 1}"#).expect_err("a document with no schema is not an entry");
        assert!(
            err.to_string().contains("schema"),
            "the refusal must say the document carries no catalogue schema, got: {err}"
        );

        let err = parse_entry(r#"{"schema": "transcribe-catalog-v1"}"#)
            .expect_err("a right-schema document with no variant is not usable");
        assert!(
            err.to_string().contains("variant"),
            "the refusal must say which field was missing, got: {err}"
        );
        assert!(parse_entry("not json").is_err(), "a non-document must be refused too");
    }

    /// The measured numbers the page compares on come off the rows
    /// themselves, and both fixtures pin the real figures.
    #[test]
    fn the_measured_facts_come_off_the_rows_the_page_compares_on() {
        let cohere = parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse");
        assert!(
            (cohere.m4_metal_xrt_wall().expect("a metal speed row") - 72.88).abs() < 1e-9,
            "cohere's m4-max metal xrt_wall, got {:?}",
            cohere.m4_metal_xrt_wall()
        );
        assert_eq!(cohere.fleurs_en_wer(), Some(5.08), "FLEURS-en at the reference quant");
        assert_eq!(
            cohere.headline_wer(),
            Some(1.27),
            "the headline row at the reference quant, not whichever row the file lists first"
        );

        let granite =
            parse_entry(&fixture("granite-speech-5.0-470m-turboctc.json")).expect("parse");
        assert!(
            (granite.m4_metal_xrt_wall().expect("a metal speed row") - 388.76).abs() < 1e-9,
            "granite's speed is the number the update line shows beside cohere's"
        );
        assert_eq!(granite.fleurs_en_wer(), Some(4.61));
        assert_eq!(granite.headline_wer(), Some(1.33));
    }

    /// The row the page's speed line draws is the row the number came
    /// from: machine, backend and quant cross with it.
    #[test]
    fn the_speed_fact_is_the_row_the_best_number_came_from() {
        let cohere = parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse");
        let row = cohere.m4_metal_speed_row().expect("a metal speed row");
        assert_eq!(row.machine, "m4-max");
        assert_eq!(row.backend, "metal");
        assert_eq!(row.quant, "Q8_0");
        assert_eq!(row.xrt_wall, Some(72.88));
    }

    /// Upstream grows the schema; an entry carrying only the required
    /// fields must still parse, with the optional facts empty.
    #[test]
    fn an_entry_with_only_the_required_fields_parses_with_empty_optionals() {
        let entry = parse_entry(r#"{"schema": "transcribe-catalog-v1", "variant": "tiny"}"#)
            .expect("the required fields are all it must carry");

        assert_eq!(entry.variant, "tiny");
        assert_eq!(entry.display_name, "tiny", "a missing display name falls back to the variant");
        assert!(entry.languages.is_empty());
        assert!(entry.downloads.is_empty());
        assert_eq!(entry.m4_metal_xrt_wall(), None, "no rows, no number");
        assert_eq!(entry.fleurs_en_wer(), None);
        assert!(!entry.streaming(), "absent capabilities are not a claim of support");
    }

    /// **The doc's marked table is the only place a download URL comes
    /// from.** The granite doc's own intro links an upstream Hugging Face
    /// repo, which is not a file forge can fetch, so a parser that took any
    /// URL in the prose would offer a dead install; the `catalog:downloads`
    /// markers are the feed's own statement of where its files are.
    #[test]
    fn parses_the_docs_marked_table_into_filenames_and_urls() {
        let raw = include_str!("../tests/fixtures/docs/granite-speech-5.0-470m-turboctc.md");
        let links = doc_links(raw);

        let expected = "granite-speech-5.0-470m-turboctc-Q4_K_M.gguf";
        let (file, url) = links
            .iter()
            .find(|(file, _)| file == expected)
            .expect("the table's Q4 row did not parse");
        assert_eq!(file, expected, "the file is the URL's own last segment");
        assert!(
            url.starts_with("https://huggingface.co/handy-computer/"),
            "the URL is the table's own, got {url}"
        );
        assert_eq!(links.len(), 6, "one link per quant row");
        assert!(
            links.iter().all(|(file, _)| std::path::Path::new(file)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))),
            "a row without a .gguf file parsed as a download"
        );
    }

    /// The second real doc, so the convention is pinned beyond one sample.
    #[test]
    fn parses_a_second_variants_table() {
        let raw = include_str!("../tests/fixtures/docs/medasr.md");
        let links = doc_links(raw);

        assert_eq!(links.len(), 6);
        assert!(
            links.iter().any(|(file, _)| file == "medasr-Q8_0.gguf"),
            "the second doc's own table did not parse"
        );
    }

    /// A doc shape this build does not understand offers no links rather
    /// than a guess - the caller reports that the doc carried no table.
    #[test]
    fn a_doc_with_no_marked_table_parses_to_nothing() {
        assert!(doc_links("# a doc with prose only").is_empty());
        assert!(
            doc_links("see https://huggingface.co/some/repo for the weights").is_empty(),
            "a URL in the prose is not a download link"
        );
    }
}

#[cfg(test)]
mod tests_catalogue_fetch {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    fn fixture(name: &str) -> String {
        let path = format!("{}/fixtures/catalogue/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).expect("the fixture must be readable")
    }

    /// **The doc is fetched from the feed's own docs tree**, one URL the
    /// source owns. A variant with no doc there answers with the HTTP
    /// failure, naming the URL, rather than an empty doc - an install
    /// reports why it could not find the file.
    #[test]
    fn fetches_a_variants_doc_and_refuses_a_missing_one() {
        let (base, _seen) = serve(vec![(
            "/docs/models/granite-speech-5.0-470m-turboctc.md",
            200,
            include_bytes!("../tests/fixtures/docs/granite-speech-5.0-470m-turboctc.md").to_vec(),
        )]);
        let source = CatalogueSource {
            listing: format!("{base}/catalog"),
            entry_base: format!("{base}/catalog/"),
            release: format!("{base}/release"),
            doc_base: format!("{base}/docs/models/"),
            repo_base: format!("{base}/repos/"),
        };

        let doc =
            fetch_doc(&source, "granite-speech-5.0-470m-turboctc").expect("the doc is served");
        assert_eq!(doc_links(&doc).len(), 6, "the fetched doc's table parses");

        let missing = fetch_doc(&source, "not-a-variant").expect_err("no doc, no fetch");
        assert!(
            format!("{missing}").contains("not-a-variant.md"),
            "the failure must name the doc it could not read, got: {missing}"
        );
    }

    /// A variant with no page in the docs tree answers from its published
    /// repo's README instead - the shape moonshine's language fine-tunes
    /// have - and a variant whose documents both fail names both URLs.
    #[test]
    fn a_variant_with_no_doc_answers_from_its_published_readme() {
        let readme = b"# moonshine-base-ar\n\n| Quant | Download |\n\
            | --- | --- |\n\
            | Q8_0 | [moonshine-base-ar-Q8_0.gguf](https://huggingface.co/handy-computer/moonshine-base-ar-gguf/resolve/main/moonshine-base-ar-Q8_0.gguf) |\n"
            .to_vec();
        let (base, _seen) = serve(vec![(
            "/repos/handy-computer/moonshine-base-ar-gguf/raw/main/README.md",
            200,
            readme,
        )]);
        let source = CatalogueSource {
            listing: format!("{base}/catalog"),
            entry_base: format!("{base}/catalog/"),
            release: format!("{base}/release"),
            doc_base: format!("{base}/docs/models/"),
            repo_base: format!("{base}/repos/"),
        };

        let links = download_links(
            &source,
            "moonshine-base-ar",
            Some("handy-computer/moonshine-base-ar-gguf"),
        )
        .expect("the README answers with the file's own URL");
        assert_eq!(links.len(), 1, "one row, one link");
        assert_eq!(links[0].0, "moonshine-base-ar-Q8_0.gguf");
        assert!(links[0].1.contains("/resolve/main/moonshine-base-ar-Q8_0.gguf"));

        let nothing =
            download_links(&source, "not-a-variant", Some("handy-computer/not-a-variant-gguf"))
                .expect_err("neither document answers");
        let message = nothing.to_string();
        assert!(
            message.contains("not-a-variant.md") && message.contains("not-a-variant-gguf"),
            "the failure names both documents it tried, got: {message}"
        );
    }

    /// Loopback HTTP/1.1 server answering fixed paths, one request per
    /// connection. Anything unrouted answers 404. Every request head it
    /// saw is readable from the returned list, which is how the
    /// User-Agent a real host demands is asserted rather than assumed.
    type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn serve(routes: Vec<(&'static str, u16, Vec<u8>)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut head = request;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                recorder.lock().unwrap().push(head);
                let (status, body) = match routes.iter().find(|(route, _, _)| *route == path) {
                    Some((_, status, body)) => (*status, body.clone()),
                    None => (404, Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (base, seen)
    }

    fn listing(names: &[&str]) -> Vec<u8> {
        let files: Vec<serde_json::Value> =
            names.iter().map(|name| serde_json::json!({"name": name, "type": "file"})).collect();
        serde_json::to_vec(&serde_json::Value::Array(files)).unwrap()
    }

    fn source(base: &str) -> CatalogueSource {
        CatalogueSource {
            listing: format!("{base}/catalog"),
            entry_base: format!("{base}/catalog/"),
            release: format!("{base}/release"),
            doc_base: format!("{base}/docs/models/"),
            repo_base: format!("{base}/repos/"),
        }
    }

    /// The real feed shape: a listing of files, one document each, plus
    /// the release the page stamps as the catalogue's version.
    #[test]
    fn every_listed_entry_is_fetched_and_parsed() {
        let (base, _seen) = serve(vec![
            ("/catalog", 200, listing(&["a.json", "b.json", "_schema.json", "README.md"])),
            ("/catalog/a.json", 200, fixture("cohere-transcribe-03-2026.json").into_bytes()),
            ("/catalog/b.json", 200, fixture("granite-speech-5.0-470m-turboctc.json").into_bytes()),
            ("/release", 200, br#"{"tag_name": "v0.3.1"}"#.to_vec()),
        ]);

        let catalogue = fetch_catalogue(&source(&base)).expect("the feed must assemble");

        assert_eq!(catalogue.entries.len(), 2, "one entry per listed document");
        assert_eq!(catalogue.skipped, 0, "nothing was skipped");
        assert_eq!(catalogue.release.as_deref(), Some("v0.3.1"), "the release tag rides along");
        assert!(!catalogue.fetched_at.is_empty(), "the fetch stamps when it ran");
        assert!(
            catalogue.entries.iter().any(|e| e.variant == "cohere-transcribe-03-2026"),
            "the entries are the documents the listing named"
        );
    }

    /// One unreadable document must not cost the whole feed: it is
    /// skipped and counted, and the count crosses.
    #[test]
    fn an_entry_that_does_not_parse_is_skipped_and_counted() {
        let (base, _seen) = serve(vec![
            ("/catalog", 200, listing(&["good.json", "broken.json"])),
            (
                "/catalog/good.json",
                200,
                fixture("granite-speech-5.0-470m-turboctc.json").into_bytes(),
            ),
            ("/catalog/broken.json", 200, b"{ not an entry".to_vec()),
            ("/release", 200, br#"{"tag_name": "v0.3.1"}"#.to_vec()),
        ]);

        let catalogue = fetch_catalogue(&source(&base)).expect("the good entry still assembles");

        assert_eq!(catalogue.entries.len(), 1);
        assert_eq!(catalogue.entries[0].variant, "granite-speech-5.0-470m-turboctc");
        assert_eq!(catalogue.skipped, 1, "the count is what tells a reader rows are missing");
    }

    /// A feed where nothing parses is a refusal: answering an empty
    /// catalogue would draw a healthy page over a broken fetch.
    #[test]
    fn a_feed_where_nothing_parses_is_refused_rather_than_answered_empty() {
        let (base, _seen) = serve(vec![
            ("/catalog", 200, listing(&["broken.json"])),
            ("/catalog/broken.json", 200, b"{}".to_vec()),
        ]);

        let err = fetch_catalogue(&source(&base)).expect_err("an empty result is not a feed");
        assert!(
            err.to_string().contains("none of the 1") && err.to_string().contains("/catalog/"),
            "the refusal must say nothing listed could be read and where it looked, got: {err}"
        );
    }

    /// The listing is the feed's spine; without it there is nothing to
    /// fetch and the status is what a reader needs to see.
    #[test]
    fn a_listing_that_answers_an_error_is_refused_with_its_status() {
        let (base, _seen) = serve(vec![("/catalog", 502, b"bad gateway".to_vec())]);

        let err = fetch_catalogue(&source(&base)).expect_err("a 502 listing is not a feed");
        assert!(
            err.to_string().contains("502"),
            "the refusal must carry the status the server answered, got: {err}"
        );
    }

    /// The release tag is a label; a feed whose release endpoint is
    /// missing still has all its entries.
    #[test]
    fn a_missing_release_is_not_a_failed_fetch() {
        let (base, _seen) = serve(vec![
            ("/catalog", 200, listing(&["a.json"])),
            ("/catalog/a.json", 200, fixture("cohere-transcribe-03-2026.json").into_bytes()),
            // /release unrouted: 404.
        ]);

        let catalogue = fetch_catalogue(&source(&base)).expect("the entries are the feed");
        assert_eq!(catalogue.entries.len(), 1);
        assert_eq!(catalogue.release, None, "no tag is None, not a failure");
    }

    /// A response past the cap is refused for that entry rather than
    /// buffered: the fetch runs on a machine-local daemon, and a broken
    /// mirror must not be able to grow its memory.
    #[test]
    fn an_oversized_entry_is_skipped_rather_than_buffered() {
        let oversized = vec![b'x'; usize::try_from(MAX_RESPONSE_BYTES).unwrap() + 1];
        let (base, _seen) = serve(vec![
            ("/catalog", 200, listing(&["huge.json", "good.json"])),
            ("/catalog/huge.json", 200, oversized),
            ("/catalog/good.json", 200, fixture("cohere-transcribe-03-2026.json").into_bytes()),
        ]);

        let catalogue = fetch_catalogue(&source(&base)).expect("the good entry assembles");
        assert_eq!(catalogue.entries.len(), 1, "the oversized one never becomes an entry");
        assert_eq!(catalogue.skipped, 1, "and it is counted as skipped");
    }

    /// A listing wide enough for several workers, with one unreadable
    /// document mid-list, must still assemble the rest and count the
    /// loss: a break in the per-chunk error arm would cost the whole
    /// worker's slice, and every narrow fixture would stay green while it
    /// did.
    #[test]
    fn a_wide_listing_assembles_across_workers_with_the_loss_counted() {
        fn tiny(variant: &str) -> Vec<u8> {
            format!(r#"{{"schema": "transcribe-catalog-v1", "variant": "{variant}"}}"#).into_bytes()
        }
        let names = [
            "e0.json", "e1.json", "e2.json", "e3.json", "e4.json", "e5.json", "e6.json", "e7.json",
        ];
        let (base, _seen) = serve(vec![
            ("/catalog", 200, listing(&names)),
            ("/catalog/e0.json", 200, tiny("e0")),
            ("/catalog/e1.json", 200, tiny("e1")),
            // Unreadable, and deliberately NOT last in its worker's
            // chunk: an error arm that abandons the slice would lose the
            // documents behind it.
            ("/catalog/e2.json", 200, b"{ not an entry".to_vec()),
            ("/catalog/e3.json", 200, tiny("e3")),
            ("/catalog/e4.json", 200, tiny("e4")),
            ("/catalog/e5.json", 200, tiny("e5")),
            ("/catalog/e6.json", 200, tiny("e6")),
            ("/catalog/e7.json", 200, tiny("e7")),
        ]);

        let catalogue = fetch_catalogue(&source(&base)).expect("the feed assembles");

        let mut variants: Vec<&str> =
            catalogue.entries.iter().map(|entry| entry.variant.as_str()).collect();
        variants.sort_unstable();
        assert_eq!(
            variants,
            ["e0", "e1", "e3", "e4", "e5", "e6", "e7"],
            "every readable document, whichever worker's slice it fell in"
        );
        assert_eq!(catalogue.skipped, 1, "and the one unreadable document is counted, not fatal");
    }

    /// api.github.com refuses any request with no User-Agent, and the
    /// listing is the spine of the fetch: a client without one reads
    /// Unreachable forever, with every call refused before a single entry
    /// is read.
    #[test]
    fn every_request_carries_a_user_agent() {
        let (base, seen) = serve(vec![
            ("/catalog", 200, listing(&["a.json"])),
            ("/catalog/a.json", 200, fixture("cohere-transcribe-03-2026.json").into_bytes()),
            ("/release", 200, br#"{"tag_name": "v0.3.1"}"#.to_vec()),
        ]);

        fetch_catalogue(&source(&base)).expect("the feed must assemble");

        let heads = seen.lock().unwrap();
        assert_eq!(heads.len(), 3, "the listing, the entry and the release: {heads:?}");
        for head in heads.iter() {
            assert!(
                head.to_ascii_lowercase().contains("user-agent: forge-dictate/"),
                "api.github.com refuses a request with no User-Agent, so every call must carry \
                 the crate's own: {head}"
            );
        }
    }

    /// The cache is what lets a page read the last-known feed offline,
    /// so it round-trips whole and an absent one is simply no catalogue.
    #[test]
    fn the_cache_round_trips_and_an_absent_one_is_no_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read_catalogue_cache(dir.path()).expect("absent is not a failure").is_none(),
            "a cache that was never written is no catalogue"
        );

        let catalogue = Catalogue {
            fetched_at: "2026-10-06T06:12:00+05:30".to_owned(),
            release: Some("v0.3.1".to_owned()),
            entries: vec![
                parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse"),
                parse_entry(&fixture("granite-speech-5.0-470m-turboctc.json")).expect("parse"),
            ],
            skipped: 1,
        };
        write_catalogue_cache(dir.path(), &catalogue).expect("the write must land");

        let back = read_catalogue_cache(dir.path())
            .expect("the write must be readable")
            .expect("a written cache is present");
        assert_eq!(back, catalogue, "the cache carries the feed as it was fetched");
    }

    /// A write that cannot complete must leave the last good cache
    /// standing: the offline read is what the cache exists for, and a
    /// truncated file is exactly what it would find after a crash
    /// mid-write.
    ///
    /// The sidecar is junked into a directory - the shape a killed
    /// process leaves - so the write cannot land. A writer that writes
    /// the cache in place would replace the good file anyway and report
    /// success.
    #[test]
    fn a_failed_cache_write_leaves_the_last_one_standing() {
        let dir = tempfile::tempdir().unwrap();
        let first = Catalogue {
            fetched_at: "2026-10-06T06:12:00Z".to_owned(),
            release: Some("v0.3.1".to_owned()),
            entries: vec![parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse")],
            skipped: 0,
        };
        write_catalogue_cache(dir.path(), &first).expect("the first write lands");

        std::fs::create_dir(dir.path().join(format!("{CATALOGUE_CACHE_FILE}.tmp")))
            .expect("the sidecar is junked");
        let second = Catalogue { fetched_at: "later".to_owned(), ..first.clone() };
        assert!(
            write_catalogue_cache(dir.path(), &second).is_err(),
            "a write that cannot land must report it rather than claim success"
        );

        let back = read_catalogue_cache(dir.path())
            .expect("the cache still reads")
            .expect("and is still there");
        assert_eq!(back, first, "the last good cache stands through the failed write");
    }

    /// A cache this build cannot read is refused by name; the caller
    /// decides whether that costs a refresh or only a warning.
    #[test]
    fn a_corrupt_cache_is_refused_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("catalogue.json"), b"{ not json").unwrap();

        let err = read_catalogue_cache(dir.path()).expect_err("a corrupt cache is not a feed");
        assert!(
            err.to_string().contains("catalogue.json"),
            "the refusal must name the file, got: {err}"
        );

        // A layout from another build is ignored rather than misread.
        let older = serde_json::json!({"version": 99, "fetched_at": "x", "entries": []});
        std::fs::write(dir.path().join("catalogue.json"), older.to_string()).unwrap();
        assert!(
            read_catalogue_cache(dir.path()).expect("a foreign layout is not an error").is_none(),
            "a cache written by another layout is no catalogue, not a misread one"
        );
    }
}
