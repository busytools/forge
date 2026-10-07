//! The cleanup feed: the normalizers the Hub carries, as the page's cleanup
//! candidates.
//!
//! **A second source, parsed rather than constructed.** The runtime's own
//! feed carries the speech models; this one is the Hub's models listing,
//! filtered by the Hub's own query (`text-normalization` + `gguf`,
//! downloads-sorted) and then by ours: a repo that only reuploads another
//! listing member collapses into it, a repo that declares languages must
//! declare English, a repo nobody has fetched is not a candidate, and a repo
//! with no quantisation this runtime loads is not one either. Every surviving
//! repo's blobs answer with each file's size AND its sha256, so unlike the
//! speech feed a cleanup download can be digest-checked.
//!
//! **The kind is what separates the two feeds' entries**, and it is
//! exhaustive: a consumer that ignores it does not compile.

use serde_json::Value;

use crate::Error;
use crate::catalogue::{
    CatalogueEntry, Download, EntryKind, License, MAX_RESPONSE_BYTES, feed_client, get_bounded_text,
};

/// Where the cleanup feed is enumerated: the Hub's models listing, with the
/// filters that are the HUB's as query parameters, and the per-repo blobs
/// endpoint that answers with the files.
#[derive(Debug, Clone)]
pub struct CleanupSource {
    /// The models listing: `text-normalization` + `gguf`, downloads-sorted.
    pub listing: String,
    /// URL a repo id is appended to for its blobs: `<blobs_base><id>?blobs=true`.
    pub blobs_base: String,
    /// URL a repo id and file are appended to for the file itself:
    /// `<files_base><id>/resolve/main/<file>`.
    pub files_base: String,
}

impl Default for CleanupSource {
    fn default() -> Self {
        Self {
            listing: "https://huggingface.co/api/models?filter=text-normalization&filter=gguf&\
                      sort=downloads&direction=-1&limit=100&full=true"
                .to_owned(),
            blobs_base: "https://huggingface.co/api/models/".to_owned(),
            files_base: "https://huggingface.co/".to_owned(),
        }
    }
}

/// The quants a candidate must offer at least one of, as file names spell
/// them - the same ladder the speech feed's rows are read at.
const QUANTS: [&str; 6] = ["F16", "Q8_0", "Q6_K", "Q5_K_M", "Q4_K_M", "Q4_K_S"];

/// The downloads floor: a repo nobody has fetched says nothing about whether
/// it works, and the listing's tail is full of them.
const DOWNLOADS_FLOOR: u64 = 100;

/// One repo as the listing describes it, before our own filters.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Repo {
    id: String,
    downloads: u64,
    license: Option<String>,
    /// Every `base_model:` target, the quantised and fine-tuned prefixes
    /// stripped, deduplicated in tag order.
    bases: Vec<String>,
    /// The two-letter codes among the tags, where the Hub mirrors the card's
    /// language field when the listing does not carry the card.
    language_tags: Vec<String>,
}

impl Repo {
    fn from_row(row: &Value) -> Option<Self> {
        let id = row.get("id")?.as_str()?.to_owned();
        let downloads = row.get("downloads").and_then(Value::as_u64).unwrap_or_default();
        let mut license = None;
        let mut bases: Vec<String> = Vec::new();
        let mut language_tags: Vec<String> = Vec::new();
        for tag in row.get("tags").and_then(Value::as_array).into_iter().flatten() {
            let Some(tag) = tag.as_str() else { continue };
            if let Some(spdx) = tag.strip_prefix("license:") {
                license = Some(spdx.to_owned());
            }
            if let Some(base) = tag.strip_prefix("base_model:") {
                let base = base.strip_prefix("quantized:").unwrap_or(base);
                let base = base.strip_prefix("finetune:").unwrap_or(base);
                if !bases.iter().any(|seen| seen == base) {
                    bases.push(base.to_owned());
                }
            }
            if tag.len() == 2 && tag.chars().all(|c| c.is_ascii_lowercase()) {
                language_tags.push(tag.to_owned());
            }
        }
        Some(Self { id, downloads, license, bases, language_tags })
    }
}

/// The repos worth offering as candidates, in the listing's own order - the
/// Hub sorts by downloads, which is the only ranking it offers.
///
/// **A reupload collapses into the family's own repo.** A reupload keeps the
/// upstream's name and names it in `base_model`, so the tag is trusted only
/// where it CORROBORATES the repo's own name; an uploader whose card names
/// some other base is making its own claim, and grouping by it would collapse
/// unrelated models. When the family's own repo is not in the listing - the
/// upstream has no GGUF build - the first reupload stands for the family,
/// which is the most downloaded one, since the listing arrives sorted.
fn candidates(rows: &[Repo]) -> Vec<Repo> {
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let mut kept: Vec<Repo> = Vec::new();
    let mut claimed: Vec<String> = Vec::new();
    for row in rows {
        let upstream = row.bases.iter().find(|base| corroborates(base, &row.id));
        if let Some(upstream) = upstream {
            if ids.contains(&upstream.as_str()) {
                // The family's own repo is here: this one is a reupload of it.
                continue;
            }
            if claimed.iter().any(|seen| seen == upstream) {
                // Another reupload of the same upstream already stands.
                continue;
            }
            claimed.push(upstream.clone());
        }
        kept.push(row.clone());
    }
    kept
}

/// Whether `base` names the repo's own model: the base's last segment
/// appears in the repo id. `superwhisper/s1-mini` corroborates
/// `Mungert/s1-mini-GGUF` where that repo names it; `Qwen/Qwen3-0.6B` does
/// not.
fn corroborates(base: &str, id: &str) -> bool {
    let name = base_name(base);
    !name.is_empty() && id.to_lowercase().contains(&name.to_lowercase())
}

/// An id's last segment: a repo's own name, or a file's own name.
fn base_name(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// The languages a repo declares: the card's own list when the blobs carried
/// one, else the two-letter codes among its tags. Empty means the card says
/// nothing, which is not a claim about English either way.
fn declared_languages(card: Option<&Value>, language_tags: &[String]) -> Vec<String> {
    let from_card: Vec<String> = card
        .and_then(|card| card.get("language"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter().filter_map(Value::as_str).map(str::to_lowercase).collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if from_card.is_empty() {
        return language_tags.to_vec();
    }
    from_card
}

/// One repo's blobs, as the entry's downloads: every file whose name carries
/// a quant this runtime loads, with its own size, digest and URL.
fn downloads_from(files: &[Value], repo: &str, files_base: &str) -> Vec<Download> {
    let mut downloads = Vec::new();
    for file in files {
        let Some(name) = file.get("rfilename").and_then(Value::as_str) else {
            continue;
        };
        if !name.to_lowercase().ends_with(".gguf") {
            continue;
        }
        let Some(quant) = quant_of(name) else {
            continue;
        };
        downloads.push(Download {
            quant,
            filename: base_name(name).to_owned(),
            size_bytes: file.get("size").and_then(Value::as_u64).unwrap_or_default(),
            sha256: file
                .get("lfs")
                .and_then(|lfs| lfs.get("sha256"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            url: Some(format!("{files_base}{repo}/resolve/main/{name}")),
        });
    }
    downloads
}

/// The quant a file name spells, when it spells one this runtime loads.
fn quant_of(name: &str) -> Option<String> {
    let upper = name.to_uppercase();
    QUANTS.iter().find(|quant| upper.contains(*quant)).map(|quant| (*quant).to_owned())
}

/// Fetch the cleanup feed. Blocking, like everything else here.
///
/// A repo whose blobs cannot be read is skipped rather than failing the
/// feed, exactly as a bad document is on the speech side; a listing that
/// answers nothing is refused, because a page drawn over an empty candidate
/// list reads the same as a page over a healthy one.
pub fn fetch_cleanup(source: &CleanupSource) -> Result<Vec<CatalogueEntry>, Error> {
    let client = feed_client(&source.listing)?;
    let listing = get_bounded_text(&client, &source.listing, MAX_RESPONSE_BYTES)?;
    let rows: Vec<Value> = serde_json::from_str(&listing)
        .map_err(|error| Error::Catalogue { message: format!("the cleanup listing: {error}") })?;
    let repos: Vec<Repo> = rows.iter().filter_map(Repo::from_row).collect();
    if repos.is_empty() {
        return Err(Error::Catalogue {
            message: format!("the listing at {} named no repos", source.listing),
        });
    }

    let mut entries = Vec::new();
    for repo in candidates(&repos) {
        if repo.downloads < DOWNLOADS_FLOOR {
            continue;
        }
        let url = format!("{}{}?blobs=true", source.blobs_base, repo.id);
        let Ok(blobs) = get_bounded_text(&client, &url, MAX_RESPONSE_BYTES) else {
            tracing::debug!(url = %url, "cleanup feed: one repo's blobs could not be read");
            continue;
        };
        let Ok(blobs) = serde_json::from_str::<Value>(&blobs) else {
            continue;
        };
        let downloads = downloads_from(
            blobs.get("siblings").and_then(Value::as_array).map_or(&[], Vec::as_slice),
            &repo.id,
            &source.files_base,
        );
        if downloads.is_empty() {
            // No quantisation this runtime loads: not a candidate.
            continue;
        }
        let languages = declared_languages(blobs.get("cardData"), &repo.language_tags);
        if !languages.is_empty() && !languages.iter().any(|language| language == "en") {
            continue;
        }
        entries.push(entry_for(&repo, downloads, languages));
    }
    Ok(entries)
}

/// One candidate as a catalogue entry, in the same shape the speech feed's
/// entries carry so the page draws both with one row.
fn entry_for(repo: &Repo, downloads: Vec<Download>, languages: Vec<String>) -> CatalogueEntry {
    CatalogueEntry {
        variant: repo.id.clone(),
        display_name: repo.id.clone(),
        family: repo.id.split_once('/').map(|(owner, _)| owner.to_owned()).unwrap_or_default(),
        params: 0,
        license: repo.license.clone().map(|spdx| License { display: spdx.clone(), spdx }),
        languages,
        capabilities: crate::catalogue::Capabilities::default(),
        headline_benchmark: None,
        published_repo: Some(repo.id.clone()),
        downloads,
        speed_benchmarks: Vec::new(),
        accuracy_benchmarks: Vec::new(),
        kind: EntryKind::Normalizer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live capture of the listing - the Hub's own answer to the default
    /// query, trimmed to the rows these rules are about.
    fn listing() -> Vec<Repo> {
        let raw = include_str!("../tests/fixtures/hf-listing.json");
        serde_json::from_str::<Vec<Value>>(raw)
            .expect("the capture parses")
            .iter()
            .filter_map(Repo::from_row)
            .collect()
    }

    /// The rows the filters keep, by id.
    fn kept(listing: &[Repo]) -> Vec<String> {
        candidates(listing).into_iter().map(|repo| repo.id).collect()
    }

    /// **A reupload collapses into the family's own repo.** The capture
    /// carries two reuploads of `s1-mini`: one names the upstream in its own
    /// `base_model` and goes, the family's own repo stands - and a repo whose
    /// card names some other base (`Qwen/Qwen3-0.6B`) is that uploader's own
    /// claim, so it neither collapses nor drags an unrelated model with it.
    #[test]
    fn a_reupload_collapses_into_the_familys_own_repo() {
        let kept = kept(&listing());

        assert!(kept.contains(&"superwhisper/s1-mini-GGUF".to_owned()), "got {kept:?}");
        assert!(
            !kept.contains(&"Joni000000000/s1-mini-de-v3".to_owned()),
            "a reupload that names its upstream collapses, got {kept:?}"
        );
        assert!(
            kept.contains(&"Mungert/s1-mini-GGUF".to_owned()),
            "a card naming a different base is its uploader's own claim, got {kept:?}"
        );
        assert!(
            kept.contains(&"DRTR-J/amnis-light-cleanup-en-v1".to_owned()),
            "an independent model that merely shares a training base stays, got {kept:?}"
        );
    }

    /// Each CeluneNorm version names its own upstream, which has no GGUF repo
    /// of its own - so each stands as its family's representative rather than
    /// all of them collapsing into one.
    #[test]
    fn a_family_with_no_repo_of_its_own_keeps_its_best_reupload() {
        let kept = kept(&listing());

        assert!(
            kept.contains(&"mradermacher/CeluneNorm-0.6B-v2.0-ctx2048-GGUF".to_owned()),
            "got {kept:?}"
        );
        assert!(
            kept.contains(&"mradermacher/CeluneNorm-0.6B-v2.0-ctx1024-GGUF".to_owned()),
            "a different upstream is a different family, got {kept:?}"
        );
    }

    /// The blobs are what turn a repo into downloads: only the files carrying
    /// a quant this runtime loads, each with its own size, digest and URL.
    #[test]
    fn the_blobs_become_downloads_with_their_own_sizes_and_digests() {
        let raw = include_str!("../tests/fixtures/hf-blobs.json");
        let blobs: Value = serde_json::from_str(raw).expect("the capture parses");
        let files = blobs.get("siblings").and_then(Value::as_array).expect("siblings");

        let downloads = downloads_from(files, "superwhisper/s1-mini-GGUF", "https://hf/");

        assert_eq!(downloads.len(), 2, "the repo's two gguf files, got {downloads:?}");
        let f16 = downloads.iter().find(|d| d.quant == "F16").expect("the f16 build");
        assert_eq!(f16.filename, "s1-mini-f16.gguf");
        assert_eq!(f16.size_bytes, 1_509_347_232);
        assert_eq!(
            f16.sha256.as_deref(),
            Some("0370da4f1bae19e3150bcafa33c5d396c15f97bf25519540a3e013db5cc00af4"),
            "the digest the Hub publishes for the file, which is the pin's own"
        );
        assert_eq!(
            f16.url.as_deref(),
            Some("https://hf/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-f16.gguf")
        );
        assert!(
            downloads.iter().all(|download| std::path::Path::new(&download.filename)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))),
            "a banner or a licence is not a download"
        );
    }

    /// The languages a repo declares come from the card the blobs carried,
    /// and only fall back to the tags when it says nothing - and the tags the
    /// fallback reads are the two-letter codes alone, never an ordinary tag
    /// that happens to be short.
    #[test]
    fn declared_languages_read_the_card_first_then_the_tags() {
        let card: Value = serde_json::from_str(r#"{"language":["en","de"]}"#).unwrap();
        assert_eq!(declared_languages(Some(&card), &["fr".to_owned()]), ["en", "de"]);

        assert_eq!(declared_languages(None, &["en".to_owned()]), ["en"]);
        assert!(
            declared_languages(Some(&serde_json::json!({})), &[]).is_empty(),
            "a card with no language declares nothing"
        );

        let row: Value = serde_json::from_str(
            r#"{"id":"a/b","downloads":1,"tags":["gguf","asr","en","license:mit"]}"#,
        )
        .unwrap();
        let repo = Repo::from_row(&row).expect("the row parses");
        assert_eq!(repo.language_tags, ["en"], "an ordinary tag is not a language");
        assert_eq!(repo.license.as_deref(), Some("mit"));
    }
}
