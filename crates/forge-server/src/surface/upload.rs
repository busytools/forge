//! The shared-file inbox: the files a client uploads into a seat's project.
//!
//! **One upload is a registry entry, a set of partial files and the bytes a
//! client streams between them.** The entry is in memory, so a connection
//! that drops mid-upload leaves it (and its partials) for a grace window and
//! a reconnected client's `file_upload_resume` continues from the offsets the
//! server already holds. A restart loses the registry, so a resume for an id
//! it never minted is refused and the client starts over.
//!
//! Partials carry a `.partial` suffix and are renamed only when the upload
//! ends, so a half-written file never appears as a shared one. Final files
//! are kept until the person or the agent deletes them: nothing sweeps the
//! inbox.
//!
//! **Every operation here is blocking I/O**, so the transport runs each one
//! on a blocking thread rather than on the connection's loop. The registry's
//! lock is held across a write, which serialises one server's uploads by
//! design: the offset check is what makes a duplicate harmless and a gap a
//! refusal, and both need the received count to be the truth at the write.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;
use forge_workspace::FileDecl;
use serde::Serialize;

/// The most one share may carry, enforced again on this side: the client's
/// own check is a courtesy, and this is the wall.
pub const MAX_UPLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// How many files one share may name. **A guard rather than a product
/// limit**: the byte cap binds the ordinary case, and a client naming a
/// million empty files would otherwise have a million partials created.
pub const MAX_FILES: usize = 256;

/// How long a dropped upload's entry and partials are kept. Long enough for
/// a phone that lost its network to reconnect and resume, and short enough
/// that an abandoned share does not sit on the disk for ever.
pub const GRACE: Duration = Duration::from_secs(60 * 60);

/// A cache name's own cap, in bytes so a unicode name cannot cross the
/// filesystem's length bound when the timestamp prefix is added.
const NAME_CAP: usize = 180;

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

/// One upload's files and where they will land.
struct Upload {
    /// The seat this upload belongs to. **Bound to it**: a resume or an end
    /// naming another seat is refused exactly as an unknown id is, so one
    /// person's upload is never written into another's project.
    seat: SessionSlot,
    files: Vec<FileState>,
    /// When the upload last moved a byte or was resumed, which is what the
    /// grace window measures from.
    touched: Instant,
}

struct FileState {
    declared: u64,
    received: u64,
    partial: PathBuf,
    final_path: PathBuf,
    handle: std::fs::File,
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

    /// Begin one upload: validate the share, create a partial per file and
    /// mint the id its chunks name.
    ///
    /// **The whole share is validated before anything is written**: a bad
    /// name, an over-cap total or a seat that cannot name a directory is a
    /// refusal with no file on disk.
    pub fn begin(&self, seat: &SessionSlot, files: &[FileDecl]) -> Result<Began, String> {
        self.sweep();
        let root = self.root.as_ref().map_err(Clone::clone)?;
        if files.is_empty() {
            return Err("the share names no files".to_owned());
        }
        if files.len() > MAX_FILES {
            return Err(format!("one share carries at most {MAX_FILES} files"));
        }
        let total: u64 = files.iter().map(|file| file.size).sum();
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
        let dir = root.join(org).join(project);
        std::fs::create_dir_all(&dir).map_err(|error| {
            format!("the inbox at {} could not be created: {error}", dir.display())
        })?;
        let stamp = timestamp();
        let mut made: Vec<FileState> = Vec::with_capacity(files.len());
        for file in files {
            match create(&dir, &stamp, &sanitised_name(&file.name)) {
                Ok((final_path, partial, handle)) => made.push(FileState {
                    declared: file.size,
                    received: 0,
                    partial,
                    final_path,
                    handle,
                }),
                Err(why) => {
                    // A share that could not be created whole leaves nothing
                    // behind: the partials already made go with the refusal.
                    for state in made {
                        let FileState { handle, partial, .. } = state;
                        drop(handle);
                        let _ = std::fs::remove_file(partial);
                    }
                    return Err(why);
                }
            }
        }
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let upload_id = mint_id(&registry)?;
        let paths = made.iter().map(|state| state.final_path.display().to_string()).collect();
        registry
            .insert(upload_id, Upload { seat: seat.clone(), files: made, touched: Instant::now() });
        Ok(Began { upload_id, paths })
    }

    /// Write one chunk at the offset it names.
    ///
    /// **The offset is what makes a retry safe**: a chunk the server already
    /// has is a duplicate and is ignored, one ahead of the received count is
    /// a gap and is refused (the socket is ordered, so a gap is a client
    /// bug), and the bytes that arrive are checked against the declared size
    /// rather than trusted.
    pub fn chunk(
        &self,
        upload_id: u64,
        file_index: u16,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), String> {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let upload =
            registry.get_mut(&upload_id).ok_or_else(|| format!("no upload {upload_id} is held"))?;
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
        file.handle
            .write_all(bytes)
            .map_err(|error| format!("file {file_index} could not be written: {error}"))?;
        file.received = end;
        upload.touched = Instant::now();
        Ok(())
    }

    /// The offsets a held upload already has, or the refusal that tells a
    /// client to begin again.
    pub fn resume(&self, seat: &SessionSlot, upload_id: u64) -> Result<Resumed, String> {
        self.sweep();
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let upload = registry
            .get_mut(&upload_id)
            .filter(|upload| &upload.seat == seat)
            .ok_or_else(unknown)?;
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
    pub fn end(&self, seat: &SessionSlot, upload_id: u64) -> Result<Ended, String> {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let upload = registry
                .get(&upload_id)
                .filter(|upload| &upload.seat == seat)
                .ok_or_else(unknown)?;
            for (index, file) in upload.files.iter().enumerate() {
                if file.received != file.declared {
                    return Err(format!(
                        "file {index} received {} of its {} declared bytes",
                        file.received, file.declared,
                    ));
                }
            }
            for file in &upload.files {
                file.handle.sync_all().map_err(|error| {
                    format!("{} could not be flushed: {error}", file.final_path.display())
                })?;
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
            if let Err(error) = std::fs::rename(&partial, &final_path) {
                let why =
                    format!("{} could not be moved into place: {error}", final_path.display());
                let _ = std::fs::remove_file(&partial);
                for rest in files {
                    let FileState { handle, partial, .. } = rest;
                    drop(handle);
                    let _ = std::fs::remove_file(partial);
                }
                return Err(why);
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
                let FileState { handle, partial, .. } = file;
                drop(handle);
                let _ = std::fs::remove_file(&partial);
            }
            tracing::debug!(
                event_name = "upload_expired",
                upload_id = id,
                "a dropped file upload passed its grace window; its partials were removed",
            );
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
    if name.contains(['/', '\\', '\0']) {
        return None;
    }
    Some(name.to_owned())
}

/// A display name made safe for the inbox: no path, no control characters,
/// never `.` or `..`, and capped so the timestamped name fits the filesystem.
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

/// Create one file's partial, answering its final path, its partial and the
/// handle.
///
/// **A collision is a name that already exists as a final file or a
/// partial**: the share gets a suffix rather than overwriting someone's
/// earlier file.
fn create(
    dir: &Path,
    stamp: &str,
    name: &str,
) -> Result<(PathBuf, PathBuf, std::fs::File), String> {
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            format!("{stamp}-{name}")
        } else {
            // The first collision is the second copy: `photo-2.jpg`.
            suffixed(stamp, name, attempt + 1)
        };
        let final_path = dir.join(&candidate);
        let mut partial_name = final_path.clone().into_os_string();
        partial_name.push(".partial");
        let partial = PathBuf::from(partial_name);
        if final_path.exists() || partial.exists() {
            continue;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&partial) {
            Ok(handle) => return Ok((final_path, partial, handle)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
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
/// shared the file; a machine with no local offset falls back to UTC rather
/// than refusing the upload.
fn timestamp() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
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

/// An id no other held upload carries, from the OS's entropy: **unguessable
/// matters**, because the id is all a resume names.
fn mint_id(registry: &HashMap<u64, Upload>) -> Result<u64, String> {
    for _ in 0..8 {
        let id =
            getrandom::u64().map_err(|error| format!("no entropy for an upload id: {error}"))?;
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

    fn feed(uploads: &Uploads, id: u64, index: u16, bytes: &[u8]) {
        for (offset, chunk) in bytes.chunks(4).enumerate() {
            uploads
                .chunk(id, index, (offset * 4) as u32, chunk)
                .expect("a chunk at the offset the server expects");
        }
    }

    /// **The inbox is per org and project, and the final name is the display
    /// name behind a stamp.** The label chooses nothing: a project's agents
    /// share one inbox.
    #[test]
    fn an_upload_lands_under_its_org_and_project() {
        let (dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("photo.jpg", 5)]).expect("a valid share begins");
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
        assert!(!path.with_extension("jpg.partial").exists(), "no partial is left behind");
    }

    /// **A partial is not a shared file.** Until the end answers, the bytes on
    /// disk carry the suffix and the final path does not exist.
    #[test]
    fn a_partial_is_renamed_only_at_the_end() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("notes.txt", 4)]).expect("begins");
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
        let began = uploads.begin(&seat(), &[decl("a.bin", 6)]).expect("begins");
        uploads.chunk(began.upload_id, 0, 0, b"abc").expect("the first chunk");
        // The retry re-sends the same bytes and continues.
        uploads.chunk(began.upload_id, 0, 0, b"abc").expect("a duplicate is ignored");
        uploads.chunk(began.upload_id, 0, 3, b"def").expect("the rest lands");
        let paths = uploads.end(&seat(), began.upload_id).expect("ends").paths;
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"abcdef");
    }

    /// A chunk ahead of the received count is a gap, and a gap is refused
    /// rather than written past.
    #[test]
    fn a_gap_is_refused() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("a.bin", 6)]).expect("begins");
        uploads.chunk(began.upload_id, 0, 0, b"abc").expect("the first chunk");
        assert!(uploads.chunk(began.upload_id, 0, 5, b"x").is_err(), "a gap is refused");
        uploads.chunk(began.upload_id, 0, 3, b"def").expect("the rest lands");
        let paths = uploads.end(&seat(), began.upload_id).expect("ends").paths;
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"abcdef", "the gap's byte never landed");
    }

    /// A file that arrives short is refused at the end, and the upload is
    /// kept so a retry can finish it rather than starting over.
    #[test]
    fn a_short_file_refuses_the_end_and_keeps_the_upload() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("a.bin", 6)]).expect("begins");
        uploads.chunk(began.upload_id, 0, 0, b"abc").expect("the first chunk");
        assert!(uploads.end(&seat(), began.upload_id).is_err(), "half a file does not end");
        let resumed = uploads.resume(&seat(), began.upload_id).expect("the upload is kept");
        assert_eq!(resumed.received, vec![3]);
    }

    /// **A resume names its offsets, and only for the seat that began it.**
    #[test]
    fn a_resume_answers_the_offsets_and_refuses_another_seat() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("a.bin", 4), decl("b.bin", 4)]).expect("begins");
        uploads.chunk(began.upload_id, 0, 0, b"ab").expect("half of the first");
        let resumed = uploads.resume(&seat(), began.upload_id).expect("the seat that began it");
        assert_eq!(resumed.received, vec![2, 0]);
        let other = SessionSlot::worker("Busytools", "forge", "w1");
        assert!(uploads.resume(&other, began.upload_id).is_err(), "another seat is refused");
        assert!(uploads.end(&other, began.upload_id).is_err(), "and cannot end it");
    }

    /// A resume for an id the server never minted is refused in words that
    /// tell the client to begin again.
    #[test]
    fn a_resume_for_an_unknown_upload_is_refused() {
        let (_dir, uploads) = uploads();
        let why = uploads.resume(&seat(), 999).expect_err("nothing is held");
        assert!(why.contains("unknown or expired"), "{why}");
    }

    /// **An abandoned upload is swept after the grace window, partials and
    /// all**, and a resume past it starts over.
    #[test]
    fn an_expired_upload_is_swept_with_its_partials() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("a.bin", 4)]).expect("begins");
        uploads.chunk(began.upload_id, 0, 0, b"ab").expect("half");
        let partial = {
            let registry = uploads.registry.lock().unwrap();
            registry[&began.upload_id].files[0].partial.clone()
        };
        assert!(partial.exists());
        {
            let mut registry = uploads.registry.lock().unwrap();
            let upload = registry.get_mut(&began.upload_id).unwrap();
            upload.touched = Instant::now() - GRACE - Duration::from_secs(1);
        }
        uploads.sweep();
        assert!(!partial.exists(), "the partial went with the entry");
        assert!(uploads.resume(&seat(), began.upload_id).is_err());
    }

    /// **The name is sanitised, never a path**: a shared file cannot choose
    /// where in the inbox it lands.
    #[test]
    fn a_name_that_is_a_path_lands_as_its_last_segment() {
        let (_dir, uploads) = uploads();
        let began = uploads
            .begin(&seat(), &[decl("../../etc/passwd", 1)])
            .expect("a path-shaped name is sanitised rather than refused");
        assert!(began.paths[0].ends_with("-passwd"), "{}", began.paths[0]);
        assert!(!began.paths[0].contains(".."), "{}", began.paths[0]);
    }

    /// A display name of dots, or one that is empty, still lands as a file.
    #[test]
    fn a_nameless_share_lands_as_a_file() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("..", 0)]).expect("begins");
        assert!(began.paths[0].ends_with("-file"), "{}", began.paths[0]);
    }

    /// Two files of one display name in one share land under two names.
    #[test]
    fn a_collision_gets_a_suffix() {
        let (_dir, uploads) = uploads();
        let began =
            uploads.begin(&seat(), &[decl("photo.jpg", 1), decl("photo.jpg", 1)]).expect("begins");
        assert_ne!(began.paths[0], began.paths[1]);
        assert!(began.paths[1].contains("-2"), "{}", began.paths[1]);
    }

    /// A share past the cap is refused before anything is created.
    #[test]
    fn an_over_cap_share_is_refused() {
        let (dir, uploads) = uploads();
        let why = uploads
            .begin(&seat(), &[decl("big.bin", MAX_UPLOAD_BYTES + 1)])
            .expect_err("over the cap");
        assert!(why.contains("at most"), "{why}");
        assert!(!dir.path().join("inbox").exists(), "nothing was created");
    }

    /// A seat whose org or project cannot be a directory name is refused
    /// rather than written somewhere else.
    #[test]
    fn a_seat_that_cannot_name_a_directory_is_refused() {
        let (_dir, uploads) = uploads();
        let odd = SessionSlot::lead("../org", "forge");
        assert!(uploads.begin(&odd, &[decl("a.bin", 1)]).is_err());
        let odd = SessionSlot::lead("Busytools", "..");
        assert!(uploads.begin(&odd, &[decl("a.bin", 1)]).is_err());
    }

    /// A chunk past the declared size is refused, even before the end check.
    #[test]
    fn a_chunk_past_the_declared_size_is_refused() {
        let (_dir, uploads) = uploads();
        let began = uploads.begin(&seat(), &[decl("a.bin", 2)]).expect("begins");
        assert!(uploads.chunk(began.upload_id, 0, 0, b"abc").is_err());
    }

    /// An upload with no app-support base is refused in the boot's words.
    #[test]
    fn an_unresolved_app_support_base_is_said_rather_than_substituted() {
        let uploads = Uploads::new(Err("no resolvable data dir".to_owned()));
        let why = uploads.begin(&seat(), &[decl("a.bin", 1)]).expect_err("no base");
        assert_eq!(why, "no resolvable data dir");
    }

    /// **The stamp is the local clock, in one shape**: a reader sorts the
    /// inbox by it, so the length is fixed even when a field is small.
    #[test]
    fn the_stamp_is_a_sortable_local_time() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(&stamp[8..9], "-", "{stamp}");
    }

    /// A long name keeps its extension and stays inside the filesystem's
    /// bound when the stamp is added.
    #[test]
    fn a_long_name_is_capped_and_keeps_its_extension() {
        let long = format!("{}.jpeg", "a".repeat(400));
        let name = sanitised_name(&long);
        assert!(name.len() <= NAME_CAP, "{} bytes", name.len());
        assert!(name.ends_with(".jpeg"), "{name}");
    }
}
