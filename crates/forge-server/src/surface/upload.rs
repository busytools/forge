//! The shared-file inbox: the files a client uploads into a seat's project.
//!
//! **One upload is a registry entry, a set of partial files and the bytes a
//! client streams between them.** The entry is in memory, so a connection
//! that drops mid-upload leaves it (and its partials) for a grace window and
//! a reconnected client's `file_upload_resume` continues from the offsets the
//! server already holds. A restart loses the registry, so a resume for an id
//! it never minted is refused and the client starts over - and the sweep
//! removes the partials a restart orphaned, by their age on disk.
//!
//! Partials carry a `.partial` suffix and are renamed only when the upload
//! ends, so a half-written file never appears as a shared one. Final files
//! are kept until the person or the agent deletes them: nothing sweeps the
//! inbox.
//!
//! **Two sets of bounds.** One share carries at most [`MAX_UPLOAD_BYTES`] over
//! [`MAX_FILES`] files; the process holds at most [`MAX_LIVE_SHARES`] live
//! shares, [`MAX_LIVE_FILES`] live partial files and [`MAX_LIVE_BYTES`] of
//! declared bytes at once, refused at begin. A per-share cap alone lets one
//! client repeat a share at message rate into permanent inbox files.
//!
//! **Every operation here is blocking I/O**, so the transport runs each one
//! on a blocking thread rather than on the connection's loop. The registry's
//! lock is held across a write, which serialises one server's uploads by
//! design: the offset check is what makes a duplicate harmless and a gap a
//! refusal, and both need the received count to be the truth at the write.
//!
//! **A write failure is remembered, not swallowed.** `write_all` can write
//! part of a chunk and then fail (a full disk, an I/O error); the registry
//! seeks before every write so the next one lands at the offset it names, and
//! it records the first failure to answer it verbatim at the end rather than
//! reporting a byte count.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use forge_primitives::SessionSlot;
use forge_workspace::FileDecl;
use serde::Serialize;

/// The most one share may carry, enforced again on this side: the client's
/// own check is a courtesy, and this is the wall.
pub const MAX_UPLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// How many files one share may name. **A product limit of its own**, beyond
/// the spec's byte cap: the byte cap binds the ordinary case, and a client
/// naming a million empty files would otherwise create a million partials.
pub const MAX_FILES: usize = 256;

/// How many shares the process holds at once, across every seat and
/// connection. A bound on the registry rather than on one share.
pub const MAX_LIVE_SHARES: usize = 8;

/// How many partial files the process holds at once. A share of empty files
/// is the case the per-share byte cap does not bind.
pub const MAX_LIVE_FILES: usize = 1024;

/// How many declared bytes the process holds at once. Bounds the disk a
/// client can claim before sending a byte.
pub const MAX_LIVE_BYTES: u64 = 1024 * 1024 * 1024;

/// How long a dropped upload's entry and partials are kept. Long enough for
/// a phone that lost its network to reconnect and resume, and short enough
/// that an abandoned share does not sit on the disk for ever.
pub const GRACE: Duration = Duration::from_secs(60 * 60);

/// A cache name's own cap, in bytes so a unicode name cannot cross the
/// filesystem's length bound when the timestamp prefix is added.
const NAME_CAP: usize = 180;

/// A seat's org or project name's own cap. They are path components and
/// prompt-visible, so they are printable and bounded.
const COMPONENT_CAP: usize = 64;

/// What a begun upload answers: the id its chunks name, and the absolute
/// paths the files will land at.
#[derive(Debug, Clone, Serialize)]
pub struct Began {
    pub upload_id: u64,
    pub paths: Vec<String>,
}

/// What a held upload answers: the same paths, and how many bytes of each one
/// the server already has.
#[derive(Debug, Clone, Serialize)]
pub struct Resumed {
    pub paths: Vec<String>,
    pub received: Vec<u64>,
}

/// What a finished upload answers: the absolute paths the files landed at.
#[derive(Debug, Clone, Serialize)]
pub struct Ended {
    pub paths: Vec<String>,
}

/// One partial file's handle, as the registry writes it.
///
/// A trait rather than a bare `File` so a test can make a write fail partway
/// and pin what the registry remembers and answers at the end.
trait Partial: Write + Seek + Send {
    fn sync(&mut self) -> std::io::Result<()>;
    fn len(&mut self) -> std::io::Result<u64>;
}

impl Partial for std::fs::File {
    fn sync(&mut self) -> std::io::Result<()> {
        self.sync_all()
    }

    fn len(&mut self) -> std::io::Result<u64> {
        self.metadata().map(|meta| meta.len())
    }
}

/// One upload's files and where they will land.
struct Upload {
    /// The seat this upload belongs to. **Bound to it**: a resume or an end
    /// naming another seat is refused exactly as an unknown id is, so one
    /// person's upload is never written into another's project.
    seat: SessionSlot,
    /// The connection that began or last resumed this upload. **A chunk is
    /// bound to it too**: the id is unguessable, but an id alone is not an
    /// authorisation - a resume re-binds the upload to the connection that
    /// reconnected, and a chunk from any other connection is refused.
    connection: u64,
    files: Vec<FileState>,
    /// When the upload last moved a byte or was resumed, which is what the
    /// grace window measures from.
    touched: Instant,
    /// The first write failure this upload met, answered verbatim at the end
    /// rather than counted as bytes that never landed.
    write_error: Option<String>,
}

struct FileState {
    declared: u64,
    received: u64,
    partial: PathBuf,
    final_path: PathBuf,
    /// Opened on the file's first chunk, never at begin: a share names up to
    /// [`MAX_FILES`] files, and holding a handle per file from begin to end
    /// would spend the process's descriptor budget on nothing.
    handle: Option<Box<dyn Partial>>,
}

/// Every upload this server holds.
pub struct Uploads {
    /// The inbox root (`<app-support>/inbox`), or the boot failure that left
    /// the app-support base unresolved. Hard rule 14: a failure is reported
    /// rather than substituted.
    root: Result<PathBuf, String>,
    registry: Mutex<HashMap<u64, Upload>>,
}

impl Uploads {
    /// The registry over an app-support base, resolved once by the workspace.
    pub fn new(app_support: Result<PathBuf, String>) -> Self {
        Self {
            root: app_support.map(|base| base.join("inbox")),
            registry: Mutex::new(HashMap::new()),
        }
    }

    /// Begin one upload: validate the share against both sets of bounds,
    /// reserve a partial per file and mint the id its chunks name.
    ///
    /// **The whole share is validated before anything is written**, and a
    /// write that could not complete leaves nothing behind.
    pub fn begin(
        &self,
        seat: &SessionSlot,
        files: &[FileDecl],
        connection: u64,
    ) -> Result<Began, String> {
        self.sweep();
        let root = self.root.as_ref().map_err(Clone::clone)?;
        if files.is_empty() {
            return Err("the share names no files".to_owned());
        }
        if files.len() > MAX_FILES {
            return Err(format!("one share carries at most {MAX_FILES} files"));
        }
        let mut total: u64 = 0;
        for file in files {
            // **One file cannot be wider than the share**, and a declared
            // size that cannot be added is refused rather than wrapped: two
            // 2^63 declarations sum to 0 in a release build.
            if file.size > MAX_UPLOAD_BYTES {
                return Err(format!(
                    "{} declares {} bytes; one file carries at most {MAX_UPLOAD_BYTES}",
                    sanitised_name(&file.name),
                    file.size,
                ));
            }
            total = total.checked_add(file.size).ok_or_else(|| {
                format!("the share's declared sizes cannot be added up at {}", file.size)
            })?;
        }
        if total > MAX_UPLOAD_BYTES {
            return Err(format!(
                "the share is {total} bytes; one share carries at most {MAX_UPLOAD_BYTES}",
            ));
        }
        let org = component(seat.org())
            .ok_or_else(|| format!("the seat's org {:?} is not a directory name", seat.org()))?;
        let project = component(seat.project()).ok_or_else(|| {
            format!("the seat's project {:?} is not a directory name", seat.project())
        })?;

        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        // **The process-wide bounds, checked against every held upload**:
        // the per-share caps bound one share, and these bound the process.
        let live_files: usize = registry.values().map(|upload| upload.files.len()).sum();
        let live_bytes: u64 = registry
            .values()
            .flat_map(|upload| upload.files.iter())
            .map(|file| file.declared)
            .sum();
        if registry.len() >= MAX_LIVE_SHARES {
            return Err(format!(
                "the server holds {MAX_LIVE_SHARES} live uploads; finish or drop one first",
            ));
        }
        if live_files + files.len() > MAX_LIVE_FILES {
            return Err(format!(
                "the server holds {MAX_LIVE_FILES} live partial files; finish or drop one first",
            ));
        }
        let claimed = live_bytes
            .checked_add(total)
            .ok_or_else(|| "the server's held bytes cannot be added up".to_owned())?;
        if claimed > MAX_LIVE_BYTES {
            return Err(format!(
                "the server holds {MAX_LIVE_BYTES} live upload bytes; finish or drop one first",
            ));
        }
        let upload_id = mint_id(&registry)?;
        let dir = root.join(org).join(project);
        std::fs::create_dir_all(&dir).map_err(|error| {
            format!("the inbox at {} could not be created: {error}", dir.display())
        })?;
        let stamp = timestamp(
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc()),
        );
        let mut made: Vec<FileState> = Vec::with_capacity(files.len());
        for file in files {
            match reserve(&dir, &stamp, &sanitised_name(&file.name), upload_id) {
                Ok((final_path, partial)) => made.push(FileState {
                    declared: file.size,
                    received: 0,
                    partial,
                    final_path,
                    handle: None,
                }),
                Err(why) => {
                    // A share that could not be reserved whole leaves nothing
                    // behind: the partials already made go with the refusal.
                    for state in made {
                        let _ = std::fs::remove_file(state.partial);
                    }
                    return Err(why);
                }
            }
        }
        let paths = made.iter().map(|state| state.final_path.display().to_string()).collect();
        registry.insert(
            upload_id,
            Upload {
                seat: seat.clone(),
                connection,
                files: made,
                touched: Instant::now(),
                write_error: None,
            },
        );
        Ok(Began { upload_id, paths })
    }

    /// Write one chunk at the offset it names.
    ///
    /// **The offset is what makes a retry safe**: a chunk the server already
    /// has is a duplicate and is ignored, one ahead of the received count is
    /// a gap and is refused (the socket is ordered, so a gap is a client
    /// bug), and the bytes that arrive are checked against the declared size
    /// rather than trusted. **The write seeks first**: a failed write can
    /// leave a partial's file position past the bytes the registry counted,
    /// and appending at that position would strand the difference in the
    /// file while the counter still passed the end check.
    pub fn chunk(
        &self,
        connection: u64,
        upload_id: u64,
        file_index: u16,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), String> {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let upload =
            registry.get_mut(&upload_id).ok_or_else(|| format!("no upload {upload_id} is held"))?;
        if upload.connection != connection {
            return Err(format!("upload {upload_id} belongs to another connection"));
        }
        let index = usize::from(file_index);
        let file = upload
            .files
            .get_mut(index)
            .ok_or_else(|| format!("upload {upload_id} has no file {file_index}"))?;
        let offset = u64::from(offset);
        if offset < file.received {
            // A duplicate: the retry re-sent bytes the server already has.
            return Ok(());
        }
        if offset > file.received {
            return Err(format!(
                "a chunk for file {file_index} arrived at {offset} with {} bytes before it",
                file.received,
            ));
        }
        let end = offset + bytes.len() as u64;
        if end > file.declared {
            return Err(format!(
                "file {file_index} would reach {end} bytes; it declared {}",
                file.declared,
            ));
        }
        if file.handle.is_none() {
            match std::fs::OpenOptions::new().write(true).open(&file.partial) {
                Ok(handle) => file.handle = Some(Box::new(handle)),
                Err(error) => {
                    let why = format!(
                        "{} could not be opened for writing: {error}",
                        file.partial.display(),
                    );
                    upload.write_error.get_or_insert_with(|| why.clone());
                    tracing::warn!(
                        event_name = "upload_open_failed",
                        seat = %upload.seat.display(),
                        partial = %file.partial.display(),
                        %error,
                        "a shared file's partial could not be opened for writing",
                    );
                    return Err(why);
                }
            }
        }
        let Some(handle) = file.handle.as_mut() else { return Err("no handle".to_owned()) };
        if let Err(error) =
            handle.seek(SeekFrom::Start(offset)).and_then(|_| handle.write_all(bytes))
        {
            // **The first failure is kept for the end**, so the client is
            // told what went wrong rather than a byte count; the file
            // position is left wherever the failed write stopped, and the
            // next chunk's seek is what puts it back on the offset.
            let why = format!("file {file_index} could not be written: {error}");
            upload.write_error.get_or_insert_with(|| why.clone());
            tracing::warn!(
                event_name = "upload_write_failed",
                seat = %upload.seat.display(),
                partial = %file.partial.display(),
                %error,
                "a chunk of a shared file could not be written",
            );
            return Err(why);
        }
        file.received = end;
        upload.touched = Instant::now();
        Ok(())
    }

    /// The offsets a held upload already has, or the refusal that tells a
    /// client to begin again. **The resume re-binds the upload to the asking
    /// connection**: a client that dropped reconnects under a new id.
    pub fn resume(
        &self,
        seat: &SessionSlot,
        upload_id: u64,
        connection: u64,
    ) -> Result<Resumed, String> {
        self.sweep();
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let upload = registry
            .get_mut(&upload_id)
            .filter(|upload| &upload.seat == seat)
            .ok_or_else(unknown)?;
        upload.connection = connection;
        upload.touched = Instant::now();
        Ok(Resumed {
            paths: upload
                .files
                .iter()
                .map(|state| state.final_path.display().to_string())
                .collect(),
            received: upload.files.iter().map(|state| state.received).collect(),
        })
    }

    /// Finish an upload: every file must have arrived whole, and the partials
    /// are then renamed into place together with the answer.
    ///
    /// **A failure leaves no half a share.** A rename that fails removes the
    /// finals already renamed and the partials still waiting, so the client
    /// starts over instead of retrying onto files that already landed.
    pub fn end(&self, seat: &SessionSlot, upload_id: u64) -> Result<Ended, String> {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let upload = registry.get_mut(&upload_id).ok_or_else(unknown)?;
            if &upload.seat != seat {
                return Err(unknown());
            }
            if let Some(why) = &upload.write_error {
                return Err(why.clone());
            }
            for (index, file) in upload.files.iter_mut().enumerate() {
                if file.received != file.declared {
                    return Err(format!(
                        "file {index} received {} of its {} declared bytes",
                        file.received, file.declared,
                    ));
                }
                // **The counter is checked against the file**, not only
                // itself: a write the registry never saw fail (a disk that
                // lied) would pass the count and fail the reader.
                let held = match file.handle.as_mut() {
                    Some(handle) => handle.len(),
                    None => std::fs::metadata(&file.partial).map(|meta| meta.len()),
                };
                match held {
                    Ok(len) if len == file.declared => {}
                    Ok(len) => {
                        return Err(format!(
                            "file {index}'s partial holds {len} bytes, not the {} it declared",
                            file.declared,
                        ));
                    }
                    Err(error) => {
                        return Err(format!(
                            "file {index}'s partial could not be measured: {error}",
                        ));
                    }
                }
                if let Some(handle) = file.handle.as_mut() {
                    handle.sync().map_err(|error| {
                        format!("{} could not be flushed: {error}", file.final_path.display())
                    })?;
                }
            }
        }
        let Some(upload) = registry.remove(&upload_id) else { return Err(unknown()) };
        let mut paths = Vec::with_capacity(upload.files.len());
        let mut files = upload.files.into_iter();
        for file in files.by_ref() {
            let FileState { handle, partial, final_path, .. } = file;
            // The handle must be closed before the rename: a file open for
            // writing on some platforms cannot be moved.
            drop(handle);
            // **A final that is there is somebody else's file**: a hard link
            // refuses it where a rename would replace it.
            match std::fs::hard_link(&partial, &final_path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&partial);
                }
                Err(error) => {
                    let shown = final_path.display();
                    let why = format!("{shown} could not be moved into place: {error}");
                    let _ = std::fs::remove_file(&partial);
                    for rest in files {
                        let _ = std::fs::remove_file(rest.partial);
                    }
                    for landed in &paths {
                        let _ = std::fs::remove_file(landed);
                    }
                    return Err(why);
                }
            }
            paths.push(final_path.display().to_string());
        }
        Ok(Ended { paths })
    }

    /// Remove every upload past its grace window, and its partials with it.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let expired: Vec<u64> = registry
            .iter()
            .filter(|(_, upload)| now.duration_since(upload.touched) > GRACE)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let Some(upload) = registry.remove(&id) else { continue };
            for file in upload.files {
                drop(file.handle);
                let _ = std::fs::remove_file(&file.partial);
            }
            tracing::debug!(
                event_name = "upload_expired",
                upload_id = id,
                "a dropped file upload passed its grace window; its partials were removed",
            );
        }
    }

    /// Remove the partials no live upload holds.
    ///
    /// **The registry is memory, so a restart orphans its partials**: on the
    /// next boot nothing knows the entries that made them, and this is the
    /// only thing that deletes them. The age is the file's own, so a partial
    /// this process is writing is never taken (it was touched a moment ago).
    pub fn sweep_orphans(&self, now: SystemTime) {
        let Ok(root) = self.root.as_ref() else { return };
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let held: std::collections::HashSet<&Path> = registry
            .values()
            .flat_map(|upload| upload.files.iter())
            .map(|file| file.partial.as_path())
            .collect();
        let mut removed = 0_usize;
        remove_orphans(root, now, &held, &mut removed);
        if removed > 0 {
            tracing::debug!(
                event_name = "upload_orphans_swept",
                removed,
                "partials no live upload held were removed after their grace window",
            );
        }
    }
}

/// Walk the inbox tree and delete `*.partial` files older than [`GRACE`] that
/// no live upload holds.
fn remove_orphans(
    dir: &Path,
    now: SystemTime,
    held: &std::collections::HashSet<&Path>,
    removed: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            remove_orphans(&path, now, held, removed);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("partial") {
            continue;
        }
        if held.contains(path.as_path()) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if now.duration_since(modified).is_ok_and(|age| age > GRACE) {
            let _ = std::fs::remove_file(&path);
            *removed += 1;
        }
    }
}

/// The refusal a resume or an end gives for an id the registry cannot use.
///
/// **One sentence for both cases on purpose**: whether an id was never minted
/// or belonged to another seat is not a client's business, and either way the
/// answer is to start again.
fn unknown() -> String {
    "that upload is unknown or expired; start it again from the beginning".to_owned()
}

/// A path component a seat names, or `None` when it cannot be one.
fn component(name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if name.chars().any(|character| character.is_control() || character == '\0') {
        return None;
    }
    if name.contains(['/', '\\']) || name.len() > COMPONENT_CAP {
        return None;
    }
    Some(name.to_owned())
}

/// A display name made safe for the inbox: no path, no control characters,
/// never `.` or `..`, and capped so the timestamped name fits the filesystem.
///
/// **A name is sanitised, not refused**: a provider's display name is not a
/// promise, and a share a person asked for should land under the closest name
/// it can have.
fn sanitised_name(display: &str) -> String {
    let last = display.rsplit(['/', '\\']).next().unwrap_or(display);
    let cleaned: String =
        last.chars().filter(|character| !character.is_control() && *character != '\0').collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "file".to_owned();
    }
    if cleaned.len() <= NAME_CAP {
        return cleaned.to_owned();
    }
    // Keep the extension when it is a short one: a truncated `photo.jpeg` is
    // still a file its reader can open by type.
    match cleaned.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 16 => {
            let room = NAME_CAP - extension.len() - 1;
            let mut keep = room.min(stem.len());
            while keep > 0 && !stem.is_char_boundary(keep) {
                keep -= 1;
            }
            format!("{}.{}", &stem[..keep], extension)
        }
        _ => {
            let mut keep = NAME_CAP;
            while keep > 0 && !cleaned.is_char_boundary(keep) {
                keep -= 1;
            }
            cleaned[..keep].to_owned()
        }
    }
}

/// Reserve one file's partial, answering its final path and its partial.
///
/// **The name is reserved, not the handle**: an empty partial is created and
/// closed, so two shares cannot choose one name, and the descriptor budget is
/// not spent until a chunk arrives. The partial carries the upload id, so one
/// share's partial can never collide with another share's final file.
fn reserve(
    dir: &Path,
    stamp: &str,
    name: &str,
    upload_id: u64,
) -> Result<(PathBuf, PathBuf), String> {
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            format!("{stamp}-{name}")
        } else {
            // The first collision is the second copy: `photo-2.jpg`.
            suffixed(stamp, name, attempt + 1)
        };
        let final_path = dir.join(&candidate);
        let partial = dir.join(format!("{candidate}.{upload_id}.partial"));
        if final_path.exists() || partial.exists() {
            continue;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&partial) {
            Ok(handle) => {
                drop(handle);
                return Ok((final_path, partial));
            }
            // Another attempt's own create won the name: try the next suffix.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "the partial for {} could not be created: {error}",
                    final_path.display(),
                ));
            }
        }
    }
    Err(format!("no free name for {name} in {}", dir.display()))
}

/// One name with its suffix before the extension: `a.jpg` -> `a-2.jpg`.
fn suffixed(stamp: &str, name: &str, attempt: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 16 => {
            format!("{stamp}-{stem}-{attempt}.{extension}")
        }
        _ => format!("{stamp}-{name}-{attempt}"),
    }
}

/// The stamp a share's names carry, in local time: `YYYYMMDD-HHMMSS`.
///
/// Local rather than UTC because a person reads the inbox by when they
/// shared the file; the stamp is the clock's, passed in so a test can pin
/// the shape rather than read it off the wall.
fn timestamp(now: time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// The bits an upload id keeps.
///
/// **53, not 64.** The id crosses the reply as JSON and a phone's page holds
/// it in a JavaScript number, which keeps every integer up to 2^53 and
/// silently rounds past that - so a 64-bit id would come back changed on a
/// retry and the resume would find nothing. 53 bits of OS entropy is far
/// past guessing for a window an hour long.
const ID_MASK: u64 = (1 << 53) - 1;

/// An id no other held upload carries, from the OS's entropy:
/// **unguessable matters**, because the id is all a resume names.
fn mint_id(registry: &HashMap<u64, Upload>) -> Result<u64, String> {
    for _ in 0..8 {
        let id =
            getrandom::u64().map_err(|error| format!("no entropy for an upload id: {error}"))?;
        let id = id & ID_MASK;
        if id != 0 && !registry.contains_key(&id) {
            return Ok(id);
        }
    }
    Err("an upload id could not be minted".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn seat() -> SessionSlot {
        SessionSlot::lead("Busytools", "forge")
    }

    fn decl(name: &str, size: u64) -> FileDecl {
        FileDecl { name: name.to_owned(), size, mime: None }
    }

    fn uploads() -> (tempfile::TempDir, Uploads) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let uploads = Uploads::new(Ok(dir.path().to_path_buf()));
        (dir, uploads)
    }

    fn begin(uploads: &Uploads, files: &[FileDecl]) -> Result<Began, String> {
        uploads.begin(&seat(), files, 1)
    }

    fn chunk(
        uploads: &Uploads,
        id: u64,
        index: u16,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), String> {
        uploads.chunk(1, id, index, offset, bytes)
    }

    fn feed(uploads: &Uploads, id: u64, index: u16, bytes: &[u8]) {
        for (chunk_index, chunk_bytes) in bytes.chunks(4).enumerate() {
            let offset = u32::try_from(chunk_index * 4).expect("this test's own offsets");
            chunk(uploads, id, index, offset, chunk_bytes)
                .expect("a chunk at the offset the server expects");
        }
    }

    /// **The inbox is per org and project, and the final name is the display
    /// name behind a stamp.** The label chooses nothing: a project's agents
    /// share one inbox.
    #[test]
    fn an_upload_lands_under_its_org_and_project() {
        let (dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("photo.jpg", 5)]).expect("a valid share begins");
        feed(&uploads, began.upload_id, 0, b"hello");
        let paths = uploads.end(&seat(), began.upload_id).expect("the upload ends").paths;
        assert_eq!(paths.len(), 1);
        let path = PathBuf::from(&paths[0]);
        assert!(path.starts_with(dir.path().join("inbox").join("Busytools").join("forge")));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(
            path.file_name().unwrap().to_string_lossy().ends_with("-photo.jpg"),
            "the display name rides behind the stamp: {}",
            path.display(),
        );
        assert!(
            !dir.path()
                .join("inbox")
                .join("Busytools")
                .join("forge")
                .join(format!(
                    "{}.{}.partial",
                    path.file_name().unwrap().to_string_lossy(),
                    began.upload_id
                ))
                .exists(),
            "no partial is left behind",
        );
    }

    /// **A partial is not a shared file.** Until the end answers, the bytes on
    /// disk carry the suffix and the final path does not exist.
    #[test]
    fn a_partial_is_renamed_only_at_the_end() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("notes.txt", 4)]).expect("begins");
        let final_path = PathBuf::from(&began.paths[0]);
        assert!(!final_path.exists(), "nothing has landed yet");
        feed(&uploads, began.upload_id, 0, b"text");
        assert!(!final_path.exists(), "the bytes are still in the partial");
        uploads.end(&seat(), began.upload_id).expect("ends");
        assert!(final_path.exists());
    }

    /// **A retry resumes from the server's offsets.** The client re-sends from
    /// a byte the server already has; the duplicate is ignored and the rest
    /// lands once, whole.
    #[test]
    fn a_duplicate_chunk_is_ignored_and_the_rest_lands() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 6)]).expect("begins");
        chunk(&uploads, began.upload_id, 0, 0, b"abc").expect("the first chunk");
        // The retry re-sends the same bytes and continues.
        chunk(&uploads, began.upload_id, 0, 0, b"abc").expect("a duplicate is ignored");
        chunk(&uploads, began.upload_id, 0, 3, b"def").expect("the rest lands");
        let paths = uploads.end(&seat(), began.upload_id).expect("ends").paths;
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"abcdef");
    }

    /// A chunk ahead of the received count is a gap, and a gap is refused
    /// rather than written past.
    #[test]
    fn a_gap_is_refused() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 6)]).expect("begins");
        chunk(&uploads, began.upload_id, 0, 0, b"abc").expect("the first chunk");
        assert!(chunk(&uploads, began.upload_id, 0, 5, b"x").is_err(), "a gap is refused");
        chunk(&uploads, began.upload_id, 0, 3, b"def").expect("the rest lands");
        let paths = uploads.end(&seat(), began.upload_id).expect("ends").paths;
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"abcdef", "the gap's byte never landed");
    }

    /// A file that arrives short is refused at the end, and the upload is
    /// kept so a retry can finish it rather than starting over.
    #[test]
    fn a_short_file_refuses_the_end_and_keeps_the_upload() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 6)]).expect("begins");
        chunk(&uploads, began.upload_id, 0, 0, b"abc").expect("the first chunk");
        assert!(uploads.end(&seat(), began.upload_id).is_err(), "half a file does not end");
        let resumed = uploads.resume(&seat(), began.upload_id, 1).expect("the upload is kept");
        assert_eq!(resumed.received, vec![3]);
    }

    /// **A resume names its offsets, and only for the seat that began it.**
    #[test]
    fn a_resume_answers_the_offsets_and_refuses_another_seat() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 4), decl("b.bin", 4)]).expect("begins");
        chunk(&uploads, began.upload_id, 0, 0, b"ab").expect("half of the first");
        let resumed = uploads.resume(&seat(), began.upload_id, 1).expect("the seat that began it");
        assert_eq!(resumed.received, vec![2, 0]);
        let other = SessionSlot::worker("Busytools", "forge", "w1");
        assert!(uploads.resume(&other, began.upload_id, 1).is_err(), "another seat is refused");
        assert!(uploads.end(&other, began.upload_id).is_err(), "and cannot end it");
    }

    /// **A chunk from another connection is refused.** The id is unguessable,
    /// but the id alone is not an authorisation; a resume re-binds the
    /// upload to the connection that reconnected.
    #[test]
    fn a_chunk_is_bound_to_the_connection_that_began_or_resumed_it() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 4)]).expect("begins");
        assert!(
            uploads.chunk(2, began.upload_id, 0, 0, b"ab").is_err(),
            "another connection's chunk is refused",
        );
        uploads.resume(&seat(), began.upload_id, 2).expect("the reconnected client resumes");
        uploads
            .chunk(2, began.upload_id, 0, 0, b"ab")
            .expect("and its chunks land after the resume");
    }

    /// A resume for an id the server never minted is refused in words that
    /// tell the client to begin again.
    #[test]
    fn a_resume_for_an_unknown_upload_is_refused() {
        let (_dir, uploads) = uploads();
        let why = uploads.resume(&seat(), 999, 1).expect_err("nothing is held");
        assert!(why.contains("unknown or expired"), "{why}");
    }

    /// A chunk for an id the server never minted is refused, not written.
    #[test]
    fn a_chunk_outside_a_begin_is_refused() {
        let (_dir, uploads) = uploads();
        assert!(chunk(&uploads, 999, 0, 0, b"ab").is_err());
    }

    /// **An upload id fits a JavaScript number.** The phone's page holds it in
    /// one, and a wider id would come back rounded on a retry, so the resume
    /// would find nothing.
    #[test]
    fn an_upload_id_fits_a_javascript_number() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 0)]).expect("begins");
        assert!(began.upload_id < (1 << 53), "{}", began.upload_id);
        assert_ne!(began.upload_id, 0);
    }

    /// **An abandoned upload is swept after the grace window, partials and
    /// all**, and a resume past it starts over. **The resume's own sweep is
    /// what takes it**: the test never calls sweep itself.
    #[test]
    fn an_expired_upload_is_swept_with_its_partials_by_the_next_resume() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 4)]).expect("begins");
        chunk(&uploads, began.upload_id, 0, 0, b"ab").expect("half");
        let partial = {
            let registry = uploads.registry.lock().unwrap();
            registry[&began.upload_id].files[0].partial.clone()
        };
        assert!(partial.exists());
        {
            let mut registry = uploads.registry.lock().unwrap();
            let upload = registry.get_mut(&began.upload_id).unwrap();
            upload.touched = Instant::now()
                .checked_sub(GRACE + Duration::from_secs(1))
                .expect("this test's own clock");
        }
        let why = uploads.resume(&seat(), began.upload_id, 1).expect_err("the resume sweeps it");
        assert!(why.contains("unknown or expired"), "{why}");
        assert!(!partial.exists(), "the partial went with the entry");
    }

    /// **A restart orphans its partials**, and the sweep takes them by their
    /// own age on disk: a fresh partial is left alone, an expired one goes.
    #[test]
    fn an_orphaned_partial_is_swept_by_its_age() {
        let (dir, uploads) = uploads();
        let project = dir.path().join("inbox").join("Busytools").join("forge");
        std::fs::create_dir_all(&project).expect("the inbox");
        let stale = project.join("20260101-000000-old.png.7.partial");
        let fresh = project.join("20260101-000000-new.png.8.partial");
        for path in [&stale, &fresh] {
            std::fs::write(path, b"x").expect("this test's own partial");
        }
        let old = SystemTime::now()
            .checked_sub(GRACE + Duration::from_secs(1))
            .expect("this test's own clock");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .expect("the stale partial")
            .set_modified(old)
            .expect("this test's own partial");
        uploads.sweep_orphans(SystemTime::now());
        assert!(!stale.exists(), "an expired orphan was left behind");
        assert!(fresh.exists(), "a fresh partial was taken");
    }

    /// **The name is sanitised, never a path**: a shared file cannot choose
    /// where in the inbox it lands.
    #[test]
    fn a_name_that_is_a_path_lands_as_its_last_segment() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("../../etc/passwd", 1)])
            .expect("a path-shaped name is sanitised rather than refused");
        assert!(began.paths[0].ends_with("-passwd"), "{}", began.paths[0]);
        assert!(!began.paths[0].contains(".."), "{}", began.paths[0]);
    }

    /// A display name of dots, or one that is empty, still lands as a file.
    #[test]
    fn a_nameless_share_lands_as_a_file() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("..", 0)]).expect("begins");
        assert!(began.paths[0].ends_with("-file"), "{}", began.paths[0]);
    }

    /// Two files of one display name in one share land under two names.
    #[test]
    fn a_collision_gets_a_suffix() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("photo.jpg", 1), decl("photo.jpg", 1)]).expect("begins");
        assert_ne!(began.paths[0], began.paths[1]);
        assert!(began.paths[1].contains("-2"), "{}", began.paths[1]);
    }

    /// A share past the cap is refused before anything is created.
    #[test]
    fn an_over_cap_share_is_refused() {
        let (dir, uploads) = uploads();
        let why =
            begin(&uploads, &[decl("big.bin", MAX_UPLOAD_BYTES + 1)]).expect_err("over the cap");
        assert!(why.contains("at most"), "{why}");
        assert!(!dir.path().join("inbox").exists(), "nothing was created");
    }

    /// **A declared size wider than the whole share is refused before any
    /// sum is taken**, so two 2^63 declarations can never wrap the byte wall
    /// to zero in a release build.
    #[test]
    fn a_declared_size_wider_than_a_share_is_refused() {
        let (_dir, uploads) = uploads();
        let why =
            begin(&uploads, &[decl("a.bin", 1 << 62), decl("b.bin", 1 << 62)]).expect_err("2^62");
        assert!(why.contains("one file carries at most"), "{why}");
    }

    /// **The process-wide bounds bind where the per-share ones do not**: a
    /// stream of small shares fills the registry and the next is refused.
    #[test]
    fn the_live_share_cap_refuses_the_next_begin() {
        let (_dir, uploads) = uploads();
        for index in 0..MAX_LIVE_SHARES {
            begin(&uploads, &[decl(&format!("a{index}.bin"), 1)]).expect("a live share");
        }
        let why = begin(&uploads, &[decl("one-more.bin", 1)]).expect_err("the registry is full");
        assert!(why.contains("live uploads"), "{why}");
    }

    /// The live file cap binds a stream of empty-file shares, which the byte
    /// cap cannot: four shares fill it, and one more file is refused.
    #[test]
    fn the_live_file_cap_refuses_a_share_of_empties() {
        let (_dir, uploads) = uploads();
        let many: Vec<FileDecl> =
            (0..MAX_FILES).map(|index| decl(&format!("e{index}.bin"), 0)).collect();
        for _ in 0..(MAX_LIVE_FILES / MAX_FILES) {
            begin(&uploads, &many).expect("a live share of empties");
        }
        let why = begin(&uploads, &[decl("one-more.bin", 0)]).expect_err("the file cap holds");
        assert!(why.contains("live partial files"), "{why}");
    }

    /// A share with no files is refused in its own words.
    #[test]
    fn an_empty_share_is_refused() {
        let (_dir, uploads) = uploads();
        let why = begin(&uploads, &[]).expect_err("no files");
        assert!(why.contains("no files"), "{why}");
    }

    /// **The 257th file is refused where the live-file cap would not bind
    /// it**: a share at the per-share limit is legal, and one past it is
    /// refused in its own words before anything is written.
    #[test]
    fn a_share_past_the_file_cap_is_refused() {
        let (_dir, uploads) = uploads();
        let at_limit: Vec<FileDecl> =
            (0..MAX_FILES).map(|index| decl(&format!("e{index}.bin"), 0)).collect();
        assert!(begin(&uploads, &at_limit).is_ok(), "a share at the limit is legal");
        let past_limit: Vec<FileDecl> =
            (0..=MAX_FILES).map(|index| decl(&format!("p{index}.bin"), 0)).collect();
        let why = begin(&uploads, &past_limit).expect_err("one file past the limit");
        assert!(why.contains("at most 256 files"), "{why}");
    }

    /// A seat whose org or project cannot be a directory name is refused
    /// rather than written somewhere else, and a control character or an
    /// unbounded name is not a directory name either.
    #[test]
    fn a_seat_that_cannot_name_a_directory_is_refused() {
        let (_dir, uploads) = uploads();
        for (org, project) in [
            ("../org", "forge"),
            ("Busytools", ".."),
            ("Busy\u{7}tools", "forge"),
            ("Busytools", &"p".repeat(COMPONENT_CAP + 1)),
        ] {
            let odd = SessionSlot::lead(org, project);
            assert!(uploads.begin(&odd, &[decl("a.bin", 1)], 1).is_err(), "{org}/{project}");
        }
    }

    /// A chunk past the declared size is refused, even before the end check.
    #[test]
    fn a_chunk_past_the_declared_size_is_refused() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 2)]).expect("begins");
        assert!(chunk(&uploads, began.upload_id, 0, 0, b"abc").is_err());
    }

    /// An upload with no app-support base is refused in the boot's words.
    #[test]
    fn an_unresolved_app_support_base_is_said_rather_than_substituted() {
        let uploads = Uploads::new(Err("no resolvable data dir".to_owned()));
        let why = uploads.begin(&seat(), &[decl("a.bin", 1)], 1).expect_err("no base");
        assert_eq!(why, "no resolvable data dir");
    }

    /// **The stamp is the clock it is given**, in one shape: a reader sorts
    /// the inbox by it, so the length and the field order are pinned.
    #[test]
    fn the_stamp_is_the_clock_it_is_given() {
        let at = time::macros::datetime!(2026-11-12 09:07:05 UTC);
        assert_eq!(timestamp(at), "20261112-090705");
    }

    /// A long name keeps its extension and stays inside the filesystem's
    /// bound when the stamp is added.
    #[test]
    fn a_long_name_is_capped_and_keeps_its_extension() {
        let long = format!("{}.jpeg", "a".repeat(400));
        let name = sanitised_name(&long);
        assert!(name.len() <= NAME_CAP, "{} bytes", name.len());
        assert!(
            std::path::Path::new(&name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jpeg")),
            "{name}",
        );
    }

    /// **A write that fails is remembered and answered at the end**, not
    /// counted as bytes that landed. The fake handle fails after writing a
    /// byte, the way a full disk does.
    #[test]
    fn a_failed_write_is_answered_at_the_end() {
        struct Failing {
            position: u64,
            declared: u64,
        }
        impl Write for Failing {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let take = buf.len().min(1);
                self.position += take as u64;
                if take == 0 {
                    return Err(std::io::Error::other("a full disk"));
                }
                Err(std::io::Error::other("a full disk after a byte"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Seek for Failing {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                if let SeekFrom::Start(at) = position {
                    self.position = at;
                }
                Ok(self.position)
            }
        }
        impl Partial for Failing {
            fn sync(&mut self) -> std::io::Result<()> {
                Ok(())
            }

            fn len(&mut self) -> std::io::Result<u64> {
                Ok(self.declared)
            }
        }

        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 4)]).expect("begins");
        {
            let mut registry = uploads.registry.lock().unwrap();
            let upload = registry.get_mut(&began.upload_id).unwrap();
            upload.files[0].handle = Some(Box::new(Failing { position: 0, declared: 4 }));
        }
        let why = chunk(&uploads, began.upload_id, 0, 0, b"abcd").expect_err("the write fails");
        assert!(why.contains("could not be written"), "{why}");
        let ended = uploads.end(&seat(), began.upload_id).expect_err("the end answers the failure");
        assert!(ended.contains("could not be written"), "{ended}");
    }

    /// **The end checks the file, not only the counter**: a partial that was
    /// truncated or grown under the registry is refused rather than renamed.
    #[test]
    fn an_end_whose_partial_does_not_hold_its_bytes_is_refused() {
        let (_dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 4)]).expect("begins");
        feed(&uploads, began.upload_id, 0, b"abcd");
        let partial = {
            let registry = uploads.registry.lock().unwrap();
            registry[&began.upload_id].files[0].partial.clone()
        };
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&partial)
            .expect("this test's own partial");
        let why =
            uploads.end(&seat(), began.upload_id).expect_err("the file does not hold its bytes");
        assert!(why.contains("holds 0 bytes"), "{why}");
    }

    /// **A rename that fails leaves no half a share**: the finals already
    /// renamed are removed with the partials still waiting, and the client
    /// starts over rather than retrying onto them.
    #[test]
    fn a_failed_rename_removes_what_already_landed() {
        let (dir, uploads) = uploads();
        let began = begin(&uploads, &[decl("a.bin", 1), decl("b.bin", 1)]).expect("begins");
        feed(&uploads, began.upload_id, 0, b"a");
        feed(&uploads, began.upload_id, 1, b"b");
        // The second final is a directory, so its hard link cannot be made.
        let second = PathBuf::from(&began.paths[1]);
        std::fs::create_dir_all(&second).expect("this test's own directory");
        let why = uploads.end(&seat(), began.upload_id).expect_err("the second rename fails");
        assert!(why.contains("could not be moved into place"), "{why}");
        assert!(!PathBuf::from(&began.paths[0]).exists(), "the first final was left behind");
        let project = dir.path().join("inbox").join("Busytools").join("forge");
        let leftovers = std::fs::read_dir(project)
            .expect("the inbox")
            .flatten()
            .filter(|entry| entry.path().to_string_lossy().ends_with(".partial"))
            .count();
        assert_eq!(leftovers, 0, "a partial was left behind");
        assert!(uploads.resume(&seat(), began.upload_id, 1).is_err(), "nothing is held");
    }
}
