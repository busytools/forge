//! The document read: one file from a session's tree, bounded, as bytes
//! with a kind.
//!
//! Resolution is against the session's own working directory, and nothing
//! outside that tree reads - the check is on the canonicalized path, so a
//! `..` walk, an absolute path and a symlink pointing out are all refused
//! the same way. The read is bounded by a ceiling and detects the kind by
//! extension. Every refusal is a sentence naming its reason, because the
//! reason is what a viewer draws and what a hand-off returns.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use forge_primitives::document::{DocumentKind, DocumentRead};

/// Why a document could not be read. Every arm is a sentence a viewer draws
/// and a hand-off returns, so the reason travels with the refusal.
#[derive(Debug, thiserror::Error)]
pub enum DocumentRefusal {
    #[error("the path resolves outside the session's tree: {path}")]
    OutsideTheTree { path: String },
    #[error("no document at {path}")]
    Missing { path: String },
    #[error("the document could not be read: {path} ({why})")]
    Unreadable { path: String, why: String },
    #[error("the document is {}, over the {} ceiling", mib(*size_bytes), mib(*ceiling_bytes))]
    OverCeiling { size_bytes: u64, ceiling_bytes: u64 },
    #[error("a PDF opens read-only; it cannot be handed reviewable")]
    PdfNotReviewable,
    #[error("the document is not UTF-8 text: {path}")]
    NotUtf8 { path: String },
}

/// Read one document out of `cwd`'s tree.
///
/// `path` resolves against `cwd`; `reviewable` rides the read back, refused
/// for a PDF because a PDF opens read-only always. The read is bounded by
/// `ceiling_bytes`, and a refusal names its reason.
pub fn read(
    cwd: &Path,
    path: &str,
    reviewable: bool,
    ceiling_bytes: u64,
) -> Result<DocumentRead, DocumentRefusal> {
    let root = cwd.canonicalize().map_err(|err| DocumentRefusal::Unreadable {
        path: cwd.display().to_string(),
        why: err.to_string(),
    })?;
    let resolved = resolve(&root, path);
    if !resolved.starts_with(&root) {
        return Err(DocumentRefusal::OutsideTheTree { path: path.to_owned() });
    }
    let metadata = std::fs::metadata(&resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            DocumentRefusal::Missing { path: path.to_owned() }
        } else {
            DocumentRefusal::Unreadable { path: path.to_owned(), why: err.to_string() }
        }
    })?;
    if !metadata.is_file() {
        return Err(DocumentRefusal::Unreadable {
            path: path.to_owned(),
            why: "not a file".to_owned(),
        });
    }
    if metadata.len() > ceiling_bytes {
        return Err(DocumentRefusal::OverCeiling { size_bytes: metadata.len(), ceiling_bytes });
    }
    let kind = kind_of(&resolved);
    if matches!(kind, DocumentKind::Pdf) && reviewable {
        return Err(DocumentRefusal::PdfNotReviewable);
    }
    let file = File::open(&resolved).map_err(|err| DocumentRefusal::Unreadable {
        path: path.to_owned(),
        why: err.to_string(),
    })?;
    // Taken rather than read whole: the ceiling bounds the allocation even
    // for a file that grew past the metadata check.
    let mut bytes = Vec::new();
    let mut bounded = file.take(ceiling_bytes + 1);
    bounded.read_to_end(&mut bytes).map_err(|err| DocumentRefusal::Unreadable {
        path: path.to_owned(),
        why: err.to_string(),
    })?;
    if bytes.len() as u64 > ceiling_bytes {
        return Err(DocumentRefusal::OverCeiling { size_bytes: bytes.len() as u64, ceiling_bytes });
    }
    if matches!(kind, DocumentKind::Markdown | DocumentKind::Text)
        && std::str::from_utf8(&bytes).is_err()
    {
        return Err(DocumentRefusal::NotUtf8 { path: path.to_owned() });
    }
    Ok(DocumentRead { path: resolved, kind, bytes, reviewable })
}

/// The canonical target of `path` under `root`. A missing file has no
/// canonical path, so its PARENT is canonicalized instead - what keeps the
/// prefix check honest for a path whose directory is a symlink out. A path
/// that canonicalizes neither way is returned as joined, which the caller's
/// existence check then refuses.
fn resolve(root: &Path, path: &str) -> PathBuf {
    let joined = root.join(path);
    if let Ok(full) = joined.canonicalize() {
        return full;
    }
    match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(name)) => {
            parent.canonicalize().map(|parent| parent.join(name)).unwrap_or(joined)
        }
        _ => joined,
    }
}

/// The kind by extension, lowercased; an extension the list does not know
/// is kept as itself.
fn kind_of(path: &Path) -> DocumentKind {
    match path.extension().and_then(std::ffi::OsStr::to_str).map(str::to_lowercase) {
        None => DocumentKind::Other(String::new()),
        Some(ext) if ext == "md" => DocumentKind::Markdown,
        Some(ext) if ext == "pdf" => DocumentKind::Pdf,
        Some(ext) if ext == "txt" => DocumentKind::Text,
        Some(ext) => DocumentKind::Other(ext),
    }
}

/// A byte count as a refusal writes it: whole MiB bare, otherwise one
/// decimal. Tenths in integer space, so no float rounding enters a
/// sentence a reader acts on.
fn mib(bytes: u64) -> String {
    const MIB: u128 = 1024 * 1024;
    let tenths = (u128::from(bytes) * 10) / MIB;
    if tenths.is_multiple_of(10) {
        format!("{} MiB", tenths / 10)
    } else {
        format!("{}.{} MiB", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use forge_primitives::document::DocumentKind;
    use tempfile::TempDir;

    use super::{DocumentRefusal, read};

    /// 1 MiB, so the over-ceiling case is small rather than a 10 MiB fixture.
    const CEILING: u64 = 1024 * 1024;

    fn fixture() -> TempDir {
        tempfile::tempdir().expect("a tempdir")
    }

    /// Write `rel` under `root`, creating parents.
    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdirs");
        }
        std::fs::write(path, bytes).expect("write fixture");
    }

    /// The control: a document inside the tree reads, resolved to its
    /// canonical absolute path - and the cwd is handed in UNCANONICALIZED,
    /// which on this machine is the `/var` -> `/private/var` case: a prefix
    /// check that skipped canonicalization would refuse every file here.
    #[test]
    fn a_document_inside_the_tree_reads_resolved_and_with_its_kind() {
        let dir = fixture();
        let root = dir.path().canonicalize().expect("the fixture canonicalizes");
        write(&root, "docs/readme.md", b"# hi\n");

        let document = read(dir.path(), "docs/readme.md", false, CEILING).expect("the read lands");

        assert_eq!(document.kind, DocumentKind::Markdown);
        assert_eq!(document.bytes, b"# hi\n");
        assert_eq!(document.path, root.join("docs/readme.md"), "the resolved absolute path");
        assert!(!document.reviewable, "the flag the hand-off passed rides the read");
    }

    /// The kinds the list starts with, by extension, case-insensitive; an
    /// extension the list does not know is kept as itself, and no extension
    /// is kept as none.
    #[test]
    fn each_extension_maps_to_its_kind_and_other_is_kept_as_itself() {
        let dir = fixture();
        write(dir.path(), "notes.md", b"x");
        write(dir.path(), "paper.PDF", b"x");
        write(dir.path(), "notes.txt", b"x");
        write(dir.path(), "sketch.rs", b"x");
        write(dir.path(), "LICENSE", b"x");

        let kind = |rel: &str| read(dir.path(), rel, false, CEILING).expect("reads").kind;
        assert_eq!(kind("notes.md"), DocumentKind::Markdown);
        assert_eq!(kind("paper.PDF"), DocumentKind::Pdf, "the extension's case does not matter");
        assert_eq!(kind("notes.txt"), DocumentKind::Text);
        assert_eq!(kind("sketch.rs"), DocumentKind::Other("rs".to_owned()));
        assert_eq!(kind("LICENSE"), DocumentKind::Other(String::new()), "no extension, none kept");
    }

    #[test]
    fn a_missing_document_is_refused_by_name() {
        let dir = fixture();

        let refusal =
            read(dir.path(), "docs/gone.md", false, CEILING).expect_err("nothing to read");

        assert!(matches!(refusal, DocumentRefusal::Missing { .. }), "{refusal:?}");
        assert!(
            refusal.to_string().contains("docs/gone.md"),
            "the refusal names the path: {refusal}"
        );
    }

    /// Over the ceiling is refused naming both sizes, and the bytes are
    /// never read: the bound holds before anything is allocated.
    #[test]
    fn a_document_over_the_ceiling_is_refused_naming_its_size() {
        let dir = fixture();
        write(dir.path(), "big.txt", &vec![b'x'; 2 * 1024 * 1024]);

        let refusal = read(dir.path(), "big.txt", false, CEILING).expect_err("over the ceiling");

        assert!(matches!(refusal, DocumentRefusal::OverCeiling { .. }), "{refusal:?}");
        let why = refusal.to_string();
        assert!(why.contains("2 MiB"), "names the document's size: {why}");
        assert!(why.contains("1 MiB"), "and the ceiling it is over: {why}");
    }

    /// Nothing outside the session's tree reads, and every shape of escape
    /// is refused BY NAME: a `..` walk, an absolute path, and a symlink that
    /// points out from inside. The targets all EXIST, so `missing` cannot be
    /// what refuses them.
    #[test]
    fn a_path_outside_the_session_tree_is_refused_by_name() {
        let dir = fixture();
        let base = dir.path().canonicalize().expect("the fixture canonicalizes");
        let cwd = base.join("tree").join("inner");
        std::fs::create_dir_all(&cwd).expect("the session tree");
        write(&base, "outside.md", b"secret");
        std::os::unix::fs::symlink(base.join("outside.md"), cwd.join("link.md"))
            .expect("the escaping symlink");
        let absolute = base.join("outside.md").to_str().expect("utf8").to_owned();

        for probe in ["../../outside.md".to_owned(), absolute, "link.md".to_owned()] {
            let refusal = read(&cwd, &probe, false, CEILING).expect_err("escapes never read");
            assert!(
                matches!(refusal, DocumentRefusal::OutsideTheTree { .. }),
                "{probe}: {refusal:?}"
            );
            assert!(refusal.to_string().contains(&probe), "the refusal names {probe}: {refusal}");
        }
    }

    /// A file the process cannot read is a refusal with the reason, not a
    /// partial document.
    #[test]
    fn an_unreadable_document_is_refused_with_the_reason() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = fixture();
        write(dir.path(), "locked.txt", b"x");
        let locked = dir.path().join("locked.txt");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("lock");

        let refusal = read(dir.path(), "locked.txt", false, CEILING).expect_err("locked");

        // Unlocked before the assert so a failing explain leaves a deletable
        // fixture behind rather than one a cleanup walks around.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).expect("unlock");
        assert!(matches!(refusal, DocumentRefusal::Unreadable { .. }), "{refusal:?}");
        assert!(
            refusal.to_string().contains("locked.txt"),
            "the refusal names the path: {refusal}"
        );
    }

    /// A PDF reads read-only - and is refused the moment the hand-off asks
    /// for it reviewable, naming the reason. The rule lives in the READ, so
    /// every door onto a document refuses with the same sentence.
    #[test]
    fn a_pdf_reads_read_only_and_a_reviewable_flag_is_refused() {
        let dir = fixture();
        write(dir.path(), "paper.pdf", b"%PDF-1.4 fake");

        let document = read(dir.path(), "paper.pdf", false, CEILING).expect("a PDF reads");
        assert_eq!(document.kind, DocumentKind::Pdf);
        assert_eq!(document.bytes, b"%PDF-1.4 fake");

        let refusal =
            read(dir.path(), "paper.pdf", true, CEILING).expect_err("reviewable is refused");
        assert!(matches!(refusal, DocumentRefusal::PdfNotReviewable), "{refusal:?}");
        assert!(refusal.to_string().contains("read-only"), "names the reason: {refusal}");
    }

    #[test]
    fn a_zero_byte_document_reads_with_no_bytes() {
        let dir = fixture();
        write(dir.path(), "empty.md", b"");

        let document =
            read(dir.path(), "empty.md", false, CEILING).expect("an empty file is a document");

        assert_eq!(document.kind, DocumentKind::Markdown);
        assert!(document.bytes.is_empty());
    }

    /// A text kind that is not UTF-8 is refused at the read rather than
    /// handed on: a viewer that decoded it would draw replacement
    /// characters, and the agent asked for a document it cannot show. An
    /// `Other` kind is never checked - its bytes are drawn as the kind's
    /// own name, not as text.
    #[test]
    fn a_non_utf8_text_document_is_refused_and_other_kinds_are_not() {
        let dir = fixture();
        let bytes = [0xff, 0xfe, 0x00, 0x01];
        write(dir.path(), "broken.md", &bytes);
        write(dir.path(), "broken.txt", &bytes);
        write(dir.path(), "blob.png", &bytes);

        for probe in ["broken.md", "broken.txt"] {
            let refusal = read(dir.path(), probe, false, CEILING).expect_err("not text");
            assert!(matches!(refusal, DocumentRefusal::NotUtf8 { .. }), "{probe}: {refusal:?}");
            assert!(refusal.to_string().contains("UTF-8"), "{probe}: names the reason: {refusal}");
        }

        let other =
            read(dir.path(), "blob.png", false, CEILING).expect("an Other kind is not text");
        assert_eq!(other.kind, DocumentKind::Other("png".to_owned()));
    }
}
