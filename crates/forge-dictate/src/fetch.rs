//! Model fetch: download, resume, and verify before use.

use std::collections::HashMap;
use std::fs::{self, File};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

use crate::{Config, Error, ModelSpec};

/// Bytes between progress reports during a transfer.
const PROGRESS_INTERVAL: u64 = 1 << 20;

/// Bytes between cancellation checkpoints while hashing. Coarse, since
/// a checkpoint round-trips the prepare driver: at hashing speed this
/// keeps cancel latency near 120 ms without an event per read.
const VERIFY_PROGRESS_INTERVAL: u64 = 64 << 20;

/// The caller's progress callback as the internals pass it around.
type Reporter<'a> = dyn FnMut(Progress) -> ControlFlow<()> + 'a;

/// Hand one progress event to the caller, turning a `Break` into the
/// error that unwinds the whole operation.
fn announce(on_progress: &mut Reporter<'_>, progress: Progress) -> Result<(), Error> {
    match on_progress(progress) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(()) => Err(Error::Cancelled),
    }
}

/// What [`prepare`] reports as it works.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Checking a file already on disk. No bytes are transferred.
    Verifying { file: String },
    /// `downloaded` of `total` bytes are now on disk for `file`.
    Downloading { file: String, downloaded: u64, total: u64 },
    /// `file` is present and verified.
    Ready { file: String },
}

/// Make every model the config names present and verified.
///
/// Blocking, like everything else here: an async caller must run this
/// on `tokio::task::spawn_blocking` or its equivalent. On a runtime
/// thread it panics in a debug build and silently holds a worker for
/// the whole transfer in a release one.
///
/// Downloads what is missing, resumes what was interrupted, and checks
/// size then SHA-256 against the [`crate::ModelSpec`] before reporting
/// a file ready. Safe to call repeatedly; a verified file is left
/// alone.
///
/// A file that is present but does not match its spec is reported, not
/// repaired. Deciding to discard someone's model file belongs to
/// whoever put it there.
///
/// Models are prepared concurrently, one thread each, so the pair costs
/// the slower of the two rather than their sum. `on_progress` is still
/// called from a single thread and one event at a time, but events from
/// the two now interleave: a caller keeping per-transfer state must key
/// it on [`Progress`]'s `file`.
///
/// Hashing is skipped for a file that has not changed since it was last
/// verified, when [`Config::digest_cache_dir`] names a place to keep
/// that record: the check becomes the stat it already needs. Without that
/// directory every call re-hashes each file end to end, measured here at
/// 0.55 s/GiB in release and 4.2 s/GiB unoptimised - the second reads as
/// a hang rather than as the profile. sha2 0.11, which this tree has
/// pinned since the sweep that moved it, is about four times faster than
/// the 0.10 before it.
pub fn prepare(
    cfg: &Config,
    mut on_progress: impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<(), Error> {
    let dir = models_dir(cfg)?;
    fs::create_dir_all(&dir).map_err(|source| Error::Io { path: dir.clone(), source })?;

    let specs: Vec<&ModelSpec> =
        std::iter::once(&cfg.asr_model).chain(cfg.normalizer.as_ref()).collect();
    let cache = DigestCache::load(cfg.digest_cache_dir.as_deref());
    // A reference, so each worker closure captures that rather than the
    // value, and this one can still write the record out below.
    let cache = &cache;
    let (report_tx, reports) = mpsc::channel::<Report>();

    let outcomes: Vec<Result<(), Error>> = std::thread::scope(|scope| {
        let workers: Vec<_> = specs
            .iter()
            .map(|spec| {
                let report_tx = report_tx.clone();
                let dir = dir.as_path();
                scope.spawn(move || {
                    // Each announcement blocks on the driver's verdict, so
                    // a `Break` still stops this transfer at the same point
                    // it would have when the callback was called directly.
                    let mut announce_to_driver = |progress| {
                        // A fresh channel per announcement, so the only
                        // sender travels WITH the report. Holding one for
                        // the worker's lifetime would keep this `recv`
                        // connected to itself, and a panicking callback
                        // would park every worker on a wait that can never
                        // end - `thread::scope` then never returns and the
                        // panic never propagates.
                        let (verdict_tx, verdict) = mpsc::channel();
                        let report = Report { progress, verdict: verdict_tx };
                        if report_tx.send(report).is_err() {
                            return ControlFlow::Break(());
                        }
                        verdict.recv().unwrap_or(ControlFlow::Break(()))
                    };
                    ensure(dir, spec, cache, &mut announce_to_driver)
                })
            })
            .collect();
        // The driver's own sender would keep `reports` open past the last
        // worker and hang the loop below.
        drop(report_tx);

        for report in reports {
            let _ = report.verdict.send(on_progress(report.progress));
        }

        workers
            .into_iter()
            .map(|worker| worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });

    // Written after the join, so a record updated by either worker is
    // included, and written even when one model failed: what verified is
    // worth keeping.
    cache.save();

    // In spec order, so which failure a caller sees does not depend on
    // which thread lost the race.
    outcomes.into_iter().find(Result::is_err).unwrap_or(Ok(()))
}

/// One model thread's progress event and the channel its verdict comes
/// back on.
struct Report {
    progress: Progress,
    verdict: mpsc::Sender<ControlFlow<()>>,
}

/// The configured directory, else a subdirectory of the platform cache
/// directory. Never derived from the working directory: a caller with
/// no usable cache directory gets an error to act on.
pub(crate) fn models_dir(cfg: &Config) -> Result<PathBuf, Error> {
    match &cfg.models_dir {
        Some(dir) => Ok(dir.clone()),
        None => dirs::cache_dir().map(|d| d.join("forge-dictate")).ok_or(Error::NoCacheDir),
    }
}

/// The record of verified digests inside
/// [`Config::digest_cache_dir`].
const DIGEST_CACHE_FILE: &str = "verified.json";

/// The record's own version. A file written by a layout this build does
/// not know is ignored rather than misread.
const DIGEST_CACHE_VERSION: u32 = 1;

/// What one file's last verification saw.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DigestRecord {
    size: u64,
    /// Modification time in nanoseconds since the epoch. Size and mtime
    /// are the whole key, so they are stored together and compared
    /// together.
    mtime_ns: u64,
    sha256: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DigestFile {
    version: u32,
    records: HashMap<PathBuf, DigestRecord>,
}

/// Remembers which digest each model file had when it was last verified,
/// so a file that has not changed since is not read end to end again.
///
/// The state a record is keyed on is the file's size and modification
/// time, both from the stat a verification needs anyway. When either
/// moves, the record no longer covers the file and the full SHA-256 runs
/// again. Bytes replaced without moving either are taken on the recorded
/// digest: that is the trade for a boot without a 3.07 GB read, and every
/// real replacement - a download, a copy, an edit - moves the mtime.
///
/// An `Err` here is never fatal. A record that cannot be read, or a
/// directory that cannot be written, costs a hash on the next run rather
/// than a failed one.
struct DigestCache {
    /// `None` disables the cache entirely, which is what a caller with
    /// no machine-local directory of its own to put it in gets.
    file: Option<PathBuf>,
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    records: HashMap<PathBuf, DigestRecord>,
    /// Whether anything was added since the last read, so a run that
    /// changed nothing does not rewrite the file.
    changed: bool,
}

impl DigestCache {
    /// Read the record beside `dir`, or start an empty one.
    fn load(dir: Option<&Path>) -> Self {
        let Some(file) = dir.map(|dir| dir.join(DIGEST_CACHE_FILE)) else {
            return Self { file: None, inner: Mutex::new(CacheInner::default()) };
        };
        let records = match fs::read_to_string(&file) {
            Ok(raw) => match serde_json::from_str::<DigestFile>(&raw) {
                Ok(parsed) if parsed.version == DIGEST_CACHE_VERSION => parsed.records,
                Ok(parsed) => {
                    tracing::warn!(
                        event_name = "dictate_digest_record_layout_unknown",
                        path = %file.display(),
                        version = parsed.version,
                        "digest record is a layout this build does not read; every model is hashed this run"
                    );
                    HashMap::new()
                }
                Err(error) => {
                    tracing::warn!(
                        event_name = "dictate_digest_record_unreadable",
                        path = %file.display(),
                        %error,
                        "digest record is unreadable; every model is hashed this run"
                    );
                    HashMap::new()
                }
            },
            // Absent is the first run, which is not a problem to report.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_digest_record_read_failed",
                    path = %file.display(),
                    %error,
                    "digest record could not be read; every model is hashed this run"
                );
                HashMap::new()
            }
        };
        Self { file: Some(file), inner: Mutex::new(CacheInner { records, changed: false }) }
    }

    /// Whether this file, at exactly this state, held these bytes when it
    /// was last verified. A digest recorded under a different spec is not
    /// a hit: re-hashing is what turns that into a mismatch the caller
    /// can act on.
    fn hit(&self, file: &Path, spec: &ModelSpec) -> bool {
        let Some((size, mtime_ns)) = state_of(file) else { return false };
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.records.get(file).is_some_and(|record| {
            record.size == size
                && record.mtime_ns == mtime_ns
                && record.sha256.eq_ignore_ascii_case(&spec.sha256)
        })
    }

    /// Record a digest computed over `file` while it was in `state`.
    ///
    /// `state` is the caller's, taken before the hash: a stat here could
    /// see a write that landed after the read and store the new state
    /// against the old bytes, which the next boot would then skip.
    fn record(&self, file: &Path, state: Option<(u64, u64)>, digest: &str) {
        if self.file.is_none() {
            return;
        }
        let Some((size, mtime_ns)) = state else { return };
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner
            .records
            .insert(file.to_path_buf(), DigestRecord { size, mtime_ns, sha256: digest.to_owned() });
        inner.changed = true;
    }

    /// Write the record back, best effort.
    fn save(&self) {
        let Some(path) = self.file.as_deref() else { return };
        let records = {
            let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if !inner.changed {
                return;
            }
            inner.records.clone()
        };
        let body = match serde_json::to_string_pretty(&DigestFile {
            version: DIGEST_CACHE_VERSION,
            records,
        }) {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_digest_record_serialize_failed",
                    %error,
                    "digest record could not be serialised; not written"
                );
                return;
            }
        };
        let written =
            path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::write(path, body));
        if let Err(error) = written {
            tracing::warn!(
                event_name = "dictate_digest_record_write_failed",
                path = %path.display(),
                %error,
                "digest record could not be written; the next run hashes what this one did"
            );
        }
    }
}

/// The state a record is keyed on: length and modification time. `None`
/// when the file cannot be stat'ed, which leaves the caller to report
/// whatever is wrong with it.
fn state_of(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let since_epoch = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((metadata.len(), u64::try_from(since_epoch.as_nanos()).ok()?))
}

fn ensure(
    dir: &Path,
    spec: &ModelSpec,
    cache: &DigestCache,
    on_progress: &mut Reporter<'_>,
) -> Result<(), Error> {
    let target = dir.join(&spec.file);
    if target.try_exists().map_err(|source| Error::Io { path: target.clone(), source })? {
        // Nothing is announced before this: a file whose state matches its
        // record is not checked again, and a "verifying" row that lives
        // for one stat is noise.
        if cache.hit(&target, spec) {
            announce(on_progress, Progress::Ready { file: spec.file.clone() })?;
            return Ok(());
        }
        announce(on_progress, Progress::Verifying { file: spec.file.clone() })?;
        let state = state_of(&target);
        let digest = verify(&target, spec, on_progress)?;
        cache.record(&target, state, &digest);
        announce(on_progress, Progress::Ready { file: spec.file.clone() })?;
        return Ok(());
    }

    let partial = dir.join(format!("{}.part", spec.file));
    download(spec, &partial, on_progress)?;

    // Announced before the hash, not after: on a multi-gigabyte file the
    // read takes seconds, and a caller left on "100%" reads it as a hang.
    announce(on_progress, Progress::Verifying { file: spec.file.clone() })?;

    let state = state_of(&partial);
    let digest = match verify(&partial, spec, on_progress) {
        Ok(digest) => digest,
        Err(failure) => return Err(discard_unusable_partial(&partial, failure)),
    };
    fs::rename(&partial, &target).map_err(|source| Error::Io { path: target.clone(), source })?;
    // One inode moved, so the bytes just hashed are the bytes now at
    // `target`, still in the state that was read before them.
    cache.record(&target, state, &digest);
    announce(on_progress, Progress::Ready { file: spec.file.clone() })?;
    Ok(())
}

/// Decide what a failed partial deserves, and say so in the log.
///
/// Only a verdict on the BYTES earns a deletion: an io error says
/// nothing about them, and a partial may predate this run, so deleting
/// on one would throw away a correct multi-gigabyte file.
fn discard_unusable_partial(partial: &Path, failure: Error) -> Error {
    if !matches!(failure, Error::SizeMismatch { .. } | Error::HashMismatch { .. }) {
        tracing::warn!(
            path = %partial.display(),
            error = %failure,
            "could not check the partial; leaving it in place"
        );
        return failure;
    }
    match fs::remove_file(partial) {
        Ok(()) => {
            tracing::warn!(
                path = %partial.display(),
                error = %failure,
                "partial does not match its spec; removed"
            );
            failure
        }
        Err(source) => {
            tracing::error!(
                path = %partial.display(),
                error = %failure,
                remove_error = %source,
                "partial does not match its spec and could not be removed"
            );
            Error::StalePartial { path: partial.into(), source }
        }
    }
}

/// Size first, then digest: a truncated file is the common case and
/// costs one `stat` to reject.
///
/// The digest compared here is always the spec's own, never a server
/// `ETag`: HuggingFace answers with a chunked xet etag that is a
/// different value from the file's SHA-256, so trusting it would reject
/// a perfectly good file forever.
///
/// Returns the digest it computed, for a caller recording it against the
/// state the file is in.
fn verify(path: &Path, spec: &ModelSpec, on_progress: &mut Reporter<'_>) -> Result<String, Error> {
    let actual =
        fs::metadata(path).map_err(|source| Error::Io { path: path.into(), source })?.len();
    if actual != spec.size {
        return Err(Error::SizeMismatch { path: path.into(), expected: spec.size, actual });
    }

    let digest = sha256(path, spec, on_progress)?;
    if !digest.eq_ignore_ascii_case(&spec.sha256) {
        return Err(Error::HashMismatch {
            path: path.into(),
            expected: spec.sha256.clone(),
            actual: digest,
        });
    }
    Ok(digest)
}

fn sha256(path: &Path, spec: &ModelSpec, on_progress: &mut Reporter<'_>) -> Result<String, Error> {
    let mut file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let mut sink = HashingWriter {
        hasher: Sha256::new(),
        name: &spec.file,
        hashed: 0,
        reported: 0,
        cancelled: false,
        interval: VERIFY_PROGRESS_INTERVAL,
        on_progress,
    };
    let copied = std::io::copy(&mut file, &mut sink);
    // Checked before the io error, because cancellation arrives AS one.
    if sink.cancelled {
        return Err(Error::Cancelled);
    }
    copied.map_err(|source| Error::Io { path: path.into(), source })?;
    Ok(hex::encode(sink.hasher.finalize()))
}

/// Feeds bytes into the digest and checkpoints cancellation every
/// `interval` bytes, so a cancel during verification does not wait out
/// the whole hash.
struct HashingWriter<'a, 'r> {
    hasher: Sha256,
    name: &'a str,
    hashed: u64,
    reported: u64,
    cancelled: bool,
    interval: u64,
    on_progress: &'a mut Reporter<'r>,
}

impl std::io::Write for HashingWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.hashed += buf.len() as u64;
        if self.hashed - self.reported >= self.interval && self.report().is_break() {
            self.cancelled = true;
            // Deliberately not `Interrupted`: write_all retries that
            // kind, and the digest cannot un-eat bytes.
            return Err(std::io::Error::other("cancelled by the progress callback"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl HashingWriter<'_, '_> {
    fn report(&mut self) -> ControlFlow<()> {
        self.reported = self.hashed;
        (self.on_progress)(Progress::Verifying { file: self.name.to_owned() })
    }
}

fn download(spec: &ModelSpec, partial: &Path, on_progress: &mut Reporter<'_>) -> Result<(), Error> {
    let mut have = match fs::metadata(partial) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(source) => return Err(Error::Io { path: partial.into(), source }),
    };
    // A partial at exactly the full length is a transfer that finished
    // and never got renamed, so hand it to verification rather than
    // fetching the whole thing again. Anything longer is not a prefix of
    // anything and starts over.
    if have == spec.size {
        return Ok(());
    }
    if have > spec.size {
        have = 0;
    }

    let http = |source| Error::Http { url: spec.url.clone(), source };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(http)?;

    let mut request = client.get(&spec.url);
    if have > 0 {
        tracing::debug!(file = %spec.file, have, "resuming interrupted download");
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut response = request.send().map_err(http)?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus { url: spec.url.clone(), status: status.as_u16() });
    }
    // Appending anything that does not begin at `have` splices two
    // overlapping copies together. Two ways that happens: a server free
    // to ignore `Range` answers 200 with the whole file, and a clamping
    // proxy answers 206 from an offset we did not ask for.
    let resuming = have > 0
        && status == reqwest::StatusCode::PARTIAL_CONTENT
        && content_range_starts_at(response.headers(), have);
    if !resuming {
        have = 0;
    }

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if resuming {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let file =
        options.open(partial).map_err(|source| Error::Io { path: partial.into(), source })?;

    let mut sink = ProgressWriter {
        file,
        name: &spec.file,
        written: have,
        reported: have,
        total: spec.size,
        cancelled: false,
        on_progress,
    };
    let copied = std::io::copy(&mut response, &mut sink);
    // Checked before the io error, because cancellation arrives AS one
    // and would otherwise be reported as a disk fault.
    if sink.cancelled {
        return Err(Error::Cancelled);
    }
    copied.map_err(|source| Error::Io { path: partial.into(), source })?;
    sink.report_now()
}

/// Parse `Content-Range: bytes <start>-<end>/<total>` and say whether
/// the body starts where the caller asked. A missing or unreadable
/// header counts as no, which costs a restart rather than a splice.
fn content_range_starts_at(headers: &reqwest::header::HeaderMap, want: u64) -> bool {
    headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_whitespace().nth(1))
        .and_then(|range| range.split('-').next())
        .and_then(|start| start.parse::<u64>().ok())
        .is_some_and(|start| start == want)
}

/// Counts bytes on their way to disk and forwards the running total,
/// rate-limited so a multi-gigabyte transfer does not call back once
/// per read.
struct ProgressWriter<'a, 'r> {
    file: File,
    name: &'a str,
    written: u64,
    reported: u64,
    total: u64,
    cancelled: bool,
    on_progress: &'a mut Reporter<'r>,
}

impl ProgressWriter<'_, '_> {
    fn report(&mut self) -> ControlFlow<()> {
        self.reported = self.written;
        (self.on_progress)(Progress::Downloading {
            file: self.name.to_owned(),
            downloaded: self.written,
            total: self.total,
        })
    }

    fn report_now(&mut self) -> Result<(), Error> {
        match self.report() {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(Error::Cancelled),
        }
    }
}

impl std::io::Write for ProgressWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.written += n as u64;
        if self.written - self.reported >= PROGRESS_INTERVAL && self.report().is_break() {
            self.cancelled = true;
            // Deliberately not `Interrupted`. `write_all` retries that
            // kind WITHOUT advancing the buffer, and these bytes are
            // already on disk, so the transfer would run to completion
            // writing every cancelled chunk twice and leave an oversized
            // partial behind. It does not spin; it corrupts.
            return Err(std::io::Error::other("cancelled by the progress callback"));
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests_cached_verification {
    use super::*;
    use crate::{ConfigBuilder, ModelFacts, ModelSpec};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::Write as _;

    /// A spec whose URL is guaranteed unreachable, so any code path
    /// that reaches the network fails loudly as [`Error::Http`]
    /// instead of quietly repairing what the test corrupted.
    fn offline_spec(file: &str, body: &[u8]) -> ModelSpec {
        ModelSpec {
            file: file.into(),
            url: "http://127.0.0.1:1/unreachable".into(),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
            facts: ModelFacts::default(),
        }
    }

    #[test]
    fn a_cached_model_matching_its_spec_is_used_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"the complete model weights";
        fs::write(dir.path().join("asr.gguf"), body).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(offline_spec("asr.gguf", body))
            .normalizer(None)
            .build();

        let mut reported = Vec::new();
        prepare(&cfg, |p| {
            reported.push(p);
            ControlFlow::Continue(())
        })
        .expect("a cached model that matches its spec must be accepted");

        let sequence: Vec<_> = reported
            .iter()
            .map(|p| match p {
                Progress::Verifying { .. } => "verifying",
                Progress::Downloading { .. } => "downloading",
                Progress::Ready { .. } => "ready",
            })
            .collect();
        assert_eq!(
            sequence,
            ["verifying", "ready"],
            "a good cached model must be verified and announced without being fetched again"
        );
    }

    #[test]
    fn a_complete_partial_is_verified_rather_than_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"the complete model weights";
        // A transfer that finished and died before the rename.
        fs::write(dir.path().join("asr.gguf.part"), body).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(offline_spec("asr.gguf", body))
            .normalizer(None)
            .build();

        prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect("a complete partial must be adopted, not re-downloaded");
        assert_eq!(
            fs::read(dir.path().join("asr.gguf")).unwrap(),
            body,
            "the finished bytes must be promoted in place rather than thrown away"
        );
    }

    /// An unreadable file is the cheapest way to make `verify` fail for
    /// a reason that says nothing about the bytes.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_partial_is_reported_not_destroyed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let body = b"the complete model weights";
        let partial = dir.path().join("asr.gguf.part");
        // Right length, so the size check passes and hashing is reached.
        fs::write(&partial, body).unwrap();
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o000)).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(offline_spec("asr.gguf", body))
            .normalizer(None)
            .build();

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("an unreadable partial cannot be verified");
        assert!(
            matches!(err, Error::Io { .. }),
            "a filesystem fault must stay an io error, not become a verdict on the bytes, got: {err:?}"
        );
        assert!(
            partial.exists(),
            "an io error says nothing about the bytes, so the file must survive it"
        );

        fs::set_permissions(&partial, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// A models directory nobody can write to: the bytes really are
    /// wrong, and the cleanup that would normally fix it cannot run.
    #[cfg(unix)]
    #[test]
    fn a_partial_that_cannot_be_removed_is_a_different_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("asr.gguf.part");
        fs::write(&partial, b"the corrupted model weight").unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(offline_spec("asr.gguf", b"the complete model weights"))
            .normalizer(None)
            .build();

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a corrupt partial must not be accepted");
        assert!(
            matches!(err, Error::StalePartial { .. }),
            "a corruption that will repeat forever must be distinguishable from one that clears on retry, got: {err:?}"
        );

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn truncated_cached_model_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = offline_spec("asr.gguf", b"the complete model weights");
        fs::write(dir.path().join("asr.gguf"), b"the complete model weight").unwrap();

        let cfg =
            ConfigBuilder::new().models_dir(dir.path()).asr_model(spec).normalizer(None).build();

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a truncated model must not be accepted");
        assert!(
            matches!(err, Error::SizeMismatch { expected: 26, actual: 25, .. }),
            "truncation must be rejected on size, got: {err:?}"
        );
    }

    #[test]
    fn cached_model_of_right_length_but_wrong_bytes_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = offline_spec("asr.gguf", b"the complete model weights");
        fs::write(dir.path().join("asr.gguf"), b"the corrupted model weight").unwrap();

        let cfg =
            ConfigBuilder::new().models_dir(dir.path()).asr_model(spec).normalizer(None).build();

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a same-length corruption must not be accepted");
        assert!(
            matches!(err, Error::HashMismatch { .. }),
            "a file of the right length with the wrong bytes must be rejected on hash, got: {err:?}"
        );
    }

    fn hashing_sink<'a, 'r>(
        interval: u64,
        on_progress: &'a mut Reporter<'r>,
    ) -> HashingWriter<'a, 'r> {
        HashingWriter {
            hasher: Sha256::new(),
            name: "asr.gguf",
            hashed: 0,
            reported: 0,
            cancelled: false,
            interval,
            on_progress,
        }
    }

    // The sink checkpoints cancellation mid-hash, so esc during
    // verification costs one interval rather than the whole digest.
    #[test]
    fn the_hashing_sink_breaks_at_its_interval() {
        let body = b"0123456789abcdefghij";
        let events = std::cell::RefCell::new(Vec::new());
        let mut on_progress = |p: Progress| {
            events.borrow_mut().push(p);
            ControlFlow::Break(())
        };
        let mut sink = hashing_sink(8, &mut on_progress);

        // Five bytes stays under the interval: no report, no error.
        sink.write_all(&body[..5]).unwrap();
        assert!(events.borrow().is_empty(), "no checkpoint under the interval");

        // Crossing 8 on the second write reports once, and the break
        // surfaces as an error carrying the cancelled flag.
        assert!(sink.write_all(&body[5..15]).is_err(), "a break must fail the write");
        assert!(sink.cancelled, "the caller must be able to tell a break from a disk fault");
        assert_eq!(sink.hashed, 15, "every fed byte reaches the count");
        assert_eq!(sink.reported, 15, "the checkpoint reported what it had hashed");
        assert_eq!(events.borrow().len(), 1, "exactly one checkpoint fired before the break");
        let first = events.borrow()[0].clone();
        assert!(
            matches!(first, Progress::Verifying { ref file } if file == "asr.gguf"),
            "the checkpoint names the file being verified, got: {first:?}"
        );
    }

    // Uncancelled, the sink is just a hasher: the digest it accumulates
    // over the fed bytes is the digest of those bytes.
    #[test]
    fn the_hashing_sink_accumulates_the_fed_digest() {
        let body = b"pretend these are recognition weights";
        let mut on_progress = |_| ControlFlow::Continue(());
        let mut sink = hashing_sink(u64::MAX, &mut on_progress);

        sink.write_all(body).unwrap();
        assert_eq!(sink.hashed, body.len() as u64, "every fed byte reaches the count");
        assert_eq!(sink.reported, 0, "an interval nothing crosses reports nothing");
        assert!(!sink.cancelled, "a continuing callback never cancels");
        assert_eq!(
            hex::encode(sink.hasher.finalize()),
            hex::encode(Sha256::digest(body)),
            "the sink must hash exactly the bytes it is fed"
        );
    }
}

#[cfg(test)]
mod tests_download {
    use super::*;
    use crate::{ConfigBuilder, ModelFacts, ModelSpec};
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Loopback HTTP/1.1 server serving a fixed set of paths and
    /// honouring `Range: bytes=N-`, so the resume path has a real 206
    /// to resume against. Every request head it saw is readable from
    /// `seen`, which is how a test proves the `Range` header was sent
    /// rather than inferring it from the resulting bytes.
    struct Server {
        base: String,
        seen: mpsc::Receiver<String>,
    }

    /// How the server answers a `Range` request. Nothing obliges it to
    /// honour one, and the two ways of not honouring it fail differently.
    #[derive(Clone, Copy)]
    enum Ranges {
        /// Serve from the requested offset, as a compliant server does.
        Honour,
        /// Answer 200 with the whole file, as a mirror or proxy may.
        Ignore,
        /// Answer 206, but from byte zero rather than where asked.
        Clamp,
    }

    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> Server {
        serve_inner(files, Ranges::Honour)
    }

    fn serve_ignoring_range(files: Vec<(&'static str, Vec<u8>)>) -> Server {
        serve_inner(files, Ranges::Ignore)
    }

    fn serve_clamping_range(files: Vec<(&'static str, Vec<u8>)>) -> Server {
        serve_inner(files, Ranges::Clamp)
    }

    fn serve_inner(files: Vec<(&'static str, Vec<u8>)>, ranges: Ranges) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, seen) = mpsc::channel();

        // Detached: nextest gives each test its own process, so the
        // accept loop dies with it and needs no shutdown channel.
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }

                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let asked: Option<usize> = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split("bytes=").nth(1))
                    .and_then(|r| r.trim().trim_end_matches('-').parse().ok());
                // Whether we answer 206, and from where, are separate
                // choices: a clamping proxy says 206 and means zero.
                let (partial, start) = match (ranges, asked) {
                    (Ranges::Honour, Some(n)) => (true, n),
                    (Ranges::Clamp, Some(_)) => (true, 0),
                    // No range asked, or one this server declines to act on.
                    (Ranges::Ignore, Some(_)) | (_, None) => (false, 0),
                };
                tx.send(head).unwrap();

                let body = files.iter().find(|(p, _)| *p == path).map(|(_, b)| b);
                let response = match body {
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                    Some(full) => {
                        let slice = &full[start.min(full.len())..];
                        let mut head = if partial {
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n",
                                slice.len(),
                                start,
                                full.len() - 1,
                                full.len()
                            )
                        } else {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", slice.len())
                        };
                        head.push_str("Connection: close\r\n\r\n");
                        let mut out = head.into_bytes();
                        out.extend_from_slice(slice);
                        out
                    }
                };
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });

        Server { base, seen }
    }

    fn spec_for(server: &Server, path: &str, body: &[u8]) -> ModelSpec {
        ModelSpec {
            file: path.trim_start_matches('/').into(),
            url: format!("{}{path}", server.base),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
            facts: ModelFacts::default(),
        }
    }

    #[test]
    fn every_configured_model_is_downloaded_and_verified() {
        let asr = b"pretend these are recognition weights".to_vec();
        let norm = b"and these rewrite the words".to_vec();
        let server = serve(vec![("/asr.gguf", asr.clone()), ("/norm.gguf", norm.clone())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &asr))
            .normalizer(spec_for(&server, "/norm.gguf", &norm))
            .build();

        let mut reported = Vec::new();
        prepare(&cfg, |p| {
            reported.push(p);
            ControlFlow::Continue(())
        })
        .expect("both models must download and verify");

        let landed_asr = fs::read(dir.path().join("asr.gguf"))
            .expect("the asr model must be on disk under its spec's file name");
        let landed_norm = fs::read(dir.path().join("norm.gguf"))
            .expect("the normalizer must be fetched too, not only the asr model");
        assert_eq!(landed_asr, asr, "the asr model must be the bytes the server served");
        assert_eq!(landed_norm, norm, "the normalizer must be the bytes the server served");

        // Sorted, not in spec order: the models are prepared concurrently,
        // so which finishes first is not a property worth pinning.
        let mut ready: Vec<_> = reported
            .iter()
            .filter_map(|p| match p {
                Progress::Ready { file } => Some(file.as_str()),
                _ => None,
            })
            .collect();
        ready.sort_unstable();
        assert_eq!(ready, ["asr.gguf", "norm.gguf"], "each model must be reported ready");
    }

    /// A panicking callback must unwind out of `prepare` rather than
    /// hang. The workers block waiting for a verdict, so if the channel
    /// they wait on can outlive the driver they park forever and
    /// `thread::scope` never joins - a hang with no message, inside a
    /// `spawn_blocking` that esc cannot reach.
    ///
    /// Watchdog rather than a bare call: the failure being guarded
    /// against is an infinite wait, and a test that reproduces it by
    /// hanging is worse than no test.
    #[test]
    fn a_panicking_callback_unwinds_rather_than_parking_the_workers() {
        let asr = b"pretend these are recognition weights".to_vec();
        let norm = b"and these rewrite the words".to_vec();
        let server = serve(vec![("/asr.gguf", asr.clone()), ("/norm.gguf", norm.clone())]);
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &asr))
            .normalizer(spec_for(&server, "/norm.gguf", &norm))
            .build();

        let running = std::thread::spawn(move || prepare(&cfg, |_| panic!("callback blew up")));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !running.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            running.is_finished(),
            "prepare never returned: the workers are parked on a verdict that cannot arrive"
        );
        assert!(
            running.join().is_err(),
            "the callback's panic must surface to the caller, not be swallowed"
        );
    }

    #[test]
    fn a_failing_model_does_not_stop_the_other_from_being_fetched() {
        let norm = b"and these rewrite the words".to_vec();
        let server = serve(vec![("/norm.gguf", norm.clone())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/absent.gguf", b"never served"))
            .normalizer(spec_for(&server, "/norm.gguf", &norm))
            .build();

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("the missing asr model must still be reported");
        assert!(
            matches!(err, Error::HttpStatus { status: 404, .. }),
            "the asr model's 404 must survive the other model succeeding, got: {err:?}"
        );
        let landed = fs::read(dir.path().join("norm.gguf")).expect(
            "the models run concurrently, so one failing must not leave the other unattempted",
        );
        assert_eq!(landed, norm, "the normalizer must be the bytes the server served");
    }

    #[test]
    fn a_download_reports_its_last_byte_before_it_reports_verifying() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        let mut reported = Vec::new();
        prepare(&cfg, |p| {
            reported.push(p);
            ControlFlow::Continue(())
        })
        .expect("the model must download");

        let total = body.len() as u64;
        let sequence: Vec<_> = reported
            .iter()
            .map(|p| match p {
                Progress::Downloading { downloaded, .. } if *downloaded == total => {
                    "downloaded-all"
                }
                Progress::Downloading { .. } => "downloading",
                Progress::Verifying { .. } => "verifying",
                Progress::Ready { .. } => "ready",
            })
            .collect();
        assert_eq!(
            sequence,
            ["downloaded-all", "verifying", "ready"],
            "a caller must be told hashing has started, or a full progress bar looks like a hang"
        );
    }

    #[test]
    fn interrupted_download_resumes_from_what_is_already_on_disk() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();
        // A previous run got this far and stopped.
        fs::write(dir.path().join("asr.gguf.part"), &body[..10]).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect("a partial download must resume, not fail");

        let head = server.seen.recv().unwrap();
        assert!(
            head.to_ascii_lowercase().contains("range: bytes=10-"),
            "resume must ask for the bytes it lacks, request was: {head}"
        );
        assert_eq!(
            fs::read(dir.path().join("asr.gguf")).unwrap(),
            body,
            "the resumed file must be the whole model, not the tail alone"
        );
    }

    #[test]
    fn a_server_that_ignores_range_does_not_produce_a_spliced_file() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve_ignoring_range(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("asr.gguf.part"), b"XXXXXXXXXX").unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect("a whole-file 200 must replace the partial, not be appended to it");
        assert_eq!(
            fs::read(dir.path().join("asr.gguf")).unwrap(),
            body,
            "a 200 answer to a ranged request must overwrite what was already on disk"
        );
    }

    #[test]
    fn breaking_mid_transfer_leaves_a_resumable_prefix() {
        // Past PROGRESS_INTERVAL, so the callback is reached from inside
        // `write` rather than only from the final report. A body under
        // the threshold never exercises the mid-write path at all.
        let body: Vec<u8> = (0..1_572_864u32).map(|i| (i % 251) as u8).collect();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        let err = prepare(&cfg, |_| ControlFlow::Break(()))
            .expect_err("a callback that breaks must stop the transfer");
        assert!(matches!(err, Error::Cancelled), "a break must surface as Cancelled, got: {err:?}");

        let partial = fs::read(dir.path().join("asr.gguf.part")).expect("the partial must remain");
        assert!(
            !partial.is_empty() && partial.len() < body.len(),
            "cancelling must stop short: kept {} of {} bytes",
            partial.len(),
            body.len()
        );
        assert_eq!(
            partial,
            body[..partial.len()],
            "what is kept must be a prefix of the file, or a later call resumes onto wrong bytes"
        );
    }

    #[test]
    fn breaking_after_the_last_byte_still_aborts() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        // Under PROGRESS_INTERVAL, so the only callback is the final
        // report after the whole body is already on disk.
        let err = prepare(&cfg, |_| ControlFlow::Break(()))
            .expect_err("a break at the closing report must still abort");
        assert!(matches!(err, Error::Cancelled), "a break must surface as Cancelled, got: {err:?}");
        assert!(
            !dir.path().join("asr.gguf").exists(),
            "a cancelled transfer must not be promoted to the real file"
        );
    }

    #[test]
    fn a_refusing_server_is_reported_as_its_status() {
        let server = serve(vec![("/asr.gguf", b"present".to_vec())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/absent.gguf", b"never served"))
            .normalizer(None)
            .build();

        let err =
            prepare(&cfg, |_| ControlFlow::Continue(())).expect_err("a 404 is not a download");
        assert!(
            matches!(err, Error::HttpStatus { status: 404, .. }),
            "a refusal must be reported as the status it was, not as a corrupt file, got: {err:?}"
        );
    }

    #[test]
    fn a_206_from_an_offset_we_did_not_ask_for_does_not_splice() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve_clamping_range(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("asr.gguf.part"), &body[..10]).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect("a 206 starting at zero must replace the partial");
        assert_eq!(
            fs::read(dir.path().join("asr.gguf")).unwrap(),
            body,
            "only the requested offset makes a 206 a resume; any other start must overwrite"
        );
    }

    #[test]
    fn partial_that_fails_verification_is_discarded() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("asr.gguf.part");
        // Right length, wrong bytes: resuming appends the correct tail
        // onto a corrupt head, so the result can never verify.
        fs::write(&partial, b"CORRUPTED!").unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a corrupt resume must not be accepted");
        assert!(
            matches!(err, Error::HashMismatch { .. }),
            "expected a hash rejection, got: {err:?}"
        );
        assert!(
            !partial.exists(),
            "a partial that cannot ever verify must be discarded, or every later resume inherits it"
        );
    }
}

#[cfg(test)]
mod tests_digest_cache {
    use super::*;
    use crate::{ConfigBuilder, ModelFacts, ModelSpec};
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    const BODY: &[u8] = b"the complete model weights";

    /// A spec whose URL is guaranteed unreachable, so any code path that
    /// reaches the network fails loudly as [`Error::Http`] instead of
    /// quietly repairing what the test changed.
    fn offline_spec(file: &str, body: &[u8]) -> ModelSpec {
        ModelSpec {
            file: file.into(),
            url: "http://127.0.0.1:1/unreachable".into(),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
            facts: ModelFacts::default(),
        }
    }

    fn config_with(models: &Path, digests: &Path, asr: ModelSpec) -> Config {
        ConfigBuilder::new()
            .models_dir(models)
            .digest_cache_dir(digests)
            .asr_model(asr)
            .normalizer(None)
            .build()
    }

    fn config(models: &Path, digests: &Path) -> Config {
        config_with(models, digests, offline_spec("asr.gguf", BODY))
    }

    fn tag(progress: &Progress) -> &'static str {
        match progress {
            Progress::Verifying { .. } => "verifying",
            Progress::Downloading { .. } => "downloading",
            Progress::Ready { .. } => "ready",
        }
    }

    fn prepare_ok(cfg: &Config) -> Vec<&'static str> {
        let mut reported = Vec::new();
        prepare(cfg, |progress| {
            reported.push(tag(&progress));
            ControlFlow::Continue(())
        })
        .expect("prepare must succeed");
        reported
    }

    /// Deny reads on a file without moving its size or its mtime, so a
    /// path that returns without reading it proves the hash was skipped
    /// and a path that reads it fails instead of quietly succeeding.
    fn deny_reads(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }

    fn allow_reads(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Move a file's mtime without touching its bytes.
    fn touch_mtime(path: &Path, when: SystemTime) {
        fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn an_unchanged_verified_file_is_not_read_again() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();
        let cfg = config(models.path(), digests.path());

        assert_eq!(
            prepare_ok(&cfg),
            ["verifying", "ready"],
            "a file with no record yet must be hashed this run"
        );

        deny_reads(&file);
        assert_eq!(
            prepare_ok(&cfg),
            ["ready"],
            "a file whose size and mtime match its record must not be read again, and the run \
             must still report it ready"
        );
        allow_reads(&file);
    }

    #[test]
    fn a_moved_mtime_is_hashed_again() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();
        let cfg = config(models.path(), digests.path());
        prepare_ok(&cfg);

        // Identical bytes delivered under a new mtime: a copy, or a
        // re-download. The state moved, so the record no longer covers it.
        touch_mtime(&file, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000));
        deny_reads(&file);
        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a file whose mtime moved must be read again");
        assert!(
            matches!(err, Error::Io { .. }),
            "the re-read must actually open the file, got: {err:?}"
        );

        allow_reads(&file);
        assert_eq!(
            prepare_ok(&cfg),
            ["verifying", "ready"],
            "and once re-verified it is usable again"
        );
    }

    #[test]
    fn a_size_change_is_rejected_with_the_mtime_restored() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();
        let cfg = config(models.path(), digests.path());
        prepare_ok(&cfg);

        // One byte longer, mtime put back to what the record holds, so
        // the size is the only field that can reject it.
        let recorded = mtime(&file);
        fs::OpenOptions::new().append(true).open(&file).unwrap().write_all(b"!").unwrap();
        touch_mtime(&file, recorded);
        assert_eq!(
            mtime(&file),
            recorded,
            "this filesystem cannot restore an mtime exactly, so this test cannot isolate the size"
        );

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a file a byte longer than its record must not be accepted");
        assert!(
            matches!(err, Error::SizeMismatch { .. }),
            "the size, not the digest, is what rejects it, got: {err:?}"
        );
    }

    /// A build that pins a new digest for a re-released model, over a
    /// file already on disk: the record covers the file's state, not the
    /// spec, so a moved spec must re-hash rather than report the old file
    /// ready under a digest this build no longer blesses.
    #[test]
    fn a_moved_spec_digest_is_not_a_hit() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();
        prepare_ok(&config(models.path(), digests.path()));

        let mut moved = offline_spec("asr.gguf", BODY);
        moved.sha256 = hex::encode(Sha256::digest(b"what the re-release weights are"));
        assert_eq!(
            moved.size,
            BODY.len() as u64,
            "only the digest may differ, or this test is about the size instead"
        );

        let err = prepare(&config_with(models.path(), digests.path(), moved), |_| {
            ControlFlow::Continue(())
        })
        .expect_err("a file whose record covers another digest must not be reported ready");

        assert!(
            matches!(err, Error::HashMismatch { .. }),
            "the moved spec must be what rejects it, and by the digest it pins, got: {err:?}"
        );
    }

    /// The direction that has to keep working: a file whose bytes were
    /// replaced is caught, because every real replacement moves the
    /// mtime with it.
    #[test]
    fn a_corrupted_file_whose_mtime_moved_is_still_rejected() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();
        let cfg = config(models.path(), digests.path());
        prepare_ok(&cfg);

        fs::write(&file, vec![b'x'; BODY.len()]).unwrap();
        touch_mtime(&file, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000));

        let err = prepare(&cfg, |_| ControlFlow::Continue(()))
            .expect_err("a file whose bytes changed must not be taken on trust");
        assert!(
            matches!(err, Error::HashMismatch { .. }),
            "the digest, not the size, is what rejects it, got: {err:?}"
        );
    }

    /// The trade this cache makes, pinned so it stays a decision: bytes
    /// replaced without moving the size or the mtime are taken on the
    /// recorded digest. Size and mtime are the whole key, which is what
    /// buys a boot without a 3.07 GB read; a download, a copy and an
    /// edit all move the mtime.
    #[test]
    fn same_size_and_mtime_is_taken_on_trust() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();
        let cfg = config(models.path(), digests.path());
        prepare_ok(&cfg);

        let recorded = mtime(&file);
        fs::write(&file, vec![b'x'; BODY.len()]).unwrap();
        touch_mtime(&file, recorded);

        assert_eq!(
            prepare_ok(&cfg),
            ["ready"],
            "a state that matches the record is not re-read, whatever the bytes are"
        );
    }

    /// The cache is an optimisation: a record that cannot be read, or a
    /// directory that cannot be written, must cost a hash rather than
    /// the run.
    #[test]
    fn an_unusable_cache_record_costs_a_hash_not_the_run() {
        let models = tempfile::tempdir().unwrap();
        let digests = tempfile::tempdir().unwrap();
        let file = models.path().join("asr.gguf");
        fs::write(&file, BODY).unwrap();

        fs::write(digests.path().join("verified.json"), b"{ not json").unwrap();
        assert_eq!(
            prepare_ok(&config(models.path(), digests.path())),
            ["verifying", "ready"],
            "an unreadable record must fall back to hashing, not fail"
        );

        let absent = digests.path().join("nothing-here-yet");
        assert_eq!(
            prepare_ok(&config(models.path(), &absent)),
            ["verifying", "ready"],
            "a record directory that does not exist yet must be created, not refused"
        );

        let blocked = tempfile::tempdir().unwrap();
        let not_a_directory = blocked.path().join("a-file");
        fs::write(&not_a_directory, b"").unwrap();
        assert_eq!(
            prepare_ok(&config(models.path(), &not_a_directory)),
            ["verifying", "ready"],
            "a record path that cannot be written must not fail the run"
        );
    }
}
