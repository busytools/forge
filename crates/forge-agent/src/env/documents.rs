//! The document read: one file from a session's tree, bounded, as bytes
//! with a kind.
//!
//! Resolution is against the session's own working directory, and nothing
//! outside that tree reads unless it sits under one of the project's
//! configured vault roots - the check is on the canonicalized path, so a
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

/// Read one document out of `cwd`'s tree, or under one of `vault_roots`.
///
/// `path` resolves against `cwd`. The read answers whether the DOCUMENT is
/// reviewable - a markdown or text kind is, and a PDF opens read-only - so a
/// viewer's door does not depend on how the page was reached. The request's
/// `reviewable` flag asks the hand-off's own question and is refused for a
/// PDF, whose kind can never carry a review. The read is bounded by
/// `ceiling_bytes`, and a refusal names its reason.
///
/// `vault_roots` are the extra roots the project allows - the
/// `vault_roots` entries of its `forge.toml` project. Each is
/// canonicalized before the comparison, like the requested path, so a
/// root behind a symlink is judged by where it really points; a root
/// that cannot be canonicalized is compared lexically, which refuses
/// nothing a resolvable path would have been allowed under.
pub fn read(
    cwd: &Path,
    vault_roots: &[PathBuf],
    path: &str,
    reviewable: bool,
    ceiling_bytes: u64,
) -> Result<DocumentRead, DocumentRefusal> {
    let root = cwd.canonicalize().map_err(|err| DocumentRefusal::Unreadable {
        path: cwd.display().to_string(),
        why: err.to_string(),
    })?;
    let resolved = resolve(&root, path);
    if !resolved.starts_with(&root) && !under_any_root(vault_roots, &resolved) {
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
    if reviewable_kind(&kind) && std::str::from_utf8(&bytes).is_err() {
        return Err(DocumentRefusal::NotUtf8 { path: path.to_owned() });
    }
    let content_hash = crate::env::git_diff::resolver::content_hash_bytes(&bytes);
    let can_review = reviewable_kind(&kind);
    Ok(DocumentRead { path: resolved, kind, bytes, reviewable: can_review, content_hash })
}

/// The extensions whose files read as reviewable text: **one set named
/// twice** with `client/src/files/languages.ts`'s `LANGS` table, which is
/// the reader's own list - the table the files page actually colours by,
/// not the chat's `NAMED`. The two cannot share a constant across the wire,
/// so each side pins its own list against a literal of the other's in a
/// test; a language added on one side is added here in the same change (the
/// lead's gate, 2026-10-17: the comment named `chat/code.ts`, which itself
/// lacked `svelte` and `kt` this list already carried, and eight client
/// extensions opened to a refusal).
const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx", "svelte", "py", "go", "json",
    "toml", "yaml", "yml", "sh", "bash", "zsh", "css", "html", "htm", "xml", "svg", "sql", "kt",
    "kts", "md", "markdown", "ini", "txt",
];

/// Whether a kind can carry a review at all: markdown, text and the code
/// kinds draw as comment targets (the files page's comments ride a
/// document read), and a PDF or an image kind opens read-only.
fn reviewable_kind(kind: &DocumentKind) -> bool {
    match kind {
        DocumentKind::Markdown | DocumentKind::Text => true,
        DocumentKind::Other(ext) => TEXT_EXTENSIONS.contains(&ext.as_str()),
        DocumentKind::Pdf => false,
    }
}

/// Whether `resolved` sits under one of the vault roots. The comparison
/// is by path component - `Path::starts_with` - so `MadnessX` is not
/// under a `Madness` root.
fn under_any_root(vault_roots: &[PathBuf], resolved: &Path) -> bool {
    vault_roots.iter().any(|vault| {
        let vault = vault.canonicalize().unwrap_or_else(|_| lexical(vault));
        resolved.starts_with(vault)
    })
}

/// The canonical target of `path` under `root`. A missing file has no
/// canonical path, so its PARENT is canonicalized instead - what keeps the
/// prefix check honest for a path whose directory is a symlink out. A path
/// that canonicalizes neither way is collapsed LEXICALLY before the prefix
/// check, so a `..` climbing past the root is still an escape when the chain
/// it climbs through is not on disk.
fn resolve(root: &Path, path: &str) -> PathBuf {
    let joined = root.join(path);
    if let Ok(full) = joined.canonicalize() {
        return full;
    }
    match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(name)) => {
            parent.canonicalize().map_or_else(|_| lexical(&joined), |parent| parent.join(name))
        }
        _ => lexical(&joined),
    }
}

/// `path` with `.` dropped and `..` popped, filesystem untouched. Popping
/// past the root is a no-op, as it is for the OS.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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

    /// **The two language lists are one set named twice.** This literal is
    /// the CLIENT's table, `client/src/files/languages.ts`'s `LANGS`, copied
    /// by hand because a constant cannot cross the wire - so a language
    /// added there is added to `TEXT_EXTENSIONS` in the same change, and the
    /// client's own test pins the same set from its side (the lead's gate,
    /// 2026-10-17: these eight - mts, cts, cjs, htm, svg, markdown, ini,
    /// kts - opened to a refusal because nothing compared the two).
    const CLIENT_LANGS: &[&str] = &[
        "rs", "ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx", "svelte", "py", "go", "json",
        "yaml", "yml", "css", "html", "htm", "xml", "svg", "sql", "md", "markdown", "toml", "ini",
        "sh", "bash", "zsh", "kt", "kts", "txt",
    ];

    /// Every extension the reader colours is reviewable, and nothing more:
    /// both directions, so neither a hole nor a kind the client cannot draw.
    #[test]
    fn the_reviewable_extensions_are_the_clients_language_table() {
        let mut ours = super::TEXT_EXTENSIONS.to_vec();
        ours.sort_unstable();
        let mut theirs = CLIENT_LANGS.to_vec();
        theirs.sort_unstable();
        assert_eq!(ours, theirs, "TEXT_EXTENSIONS and the client's LANGS are one set named twice");
    }

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

        let document =
            read(dir.path(), &[], "docs/readme.md", false, CEILING).expect("the read lands");

        assert_eq!(document.kind, DocumentKind::Markdown);
        assert_eq!(document.bytes, b"# hi\n");
        assert_eq!(document.path, root.join("docs/readme.md"), "the resolved absolute path");
        assert!(document.reviewable, "a markdown document is reviewable whoever opened it");
        assert_eq!(
            document.content_hash,
            crate::env::git_diff::resolver::content_hash_bytes(b"# hi\n"),
            "and the document's own hash, which a thread's anchor compares against",
        );
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

        let kind = |rel: &str| read(dir.path(), &[], rel, false, CEILING).expect("reads").kind;
        assert_eq!(kind("notes.md"), DocumentKind::Markdown);
        assert_eq!(kind("paper.PDF"), DocumentKind::Pdf, "the extension's case does not matter");
        assert_eq!(kind("notes.txt"), DocumentKind::Text);
        assert_eq!(kind("sketch.rs"), DocumentKind::Other("rs".to_owned()));
        assert_eq!(kind("LICENSE"), DocumentKind::Other(String::new()), "no extension, none kept");
    }

    /// **The read answers whether the DOCUMENT is reviewable, not what the
    /// caller asked.** A markdown, text or code document a viewer opened
    /// cold - the request flag false - is reviewable all the same, so the
    /// files page draws its comments however the reader reached it; a PDF
    /// and an image kind are not, whatever the caller asked for.
    #[test]
    fn the_read_answers_the_documents_own_reviewability_not_the_callers_flag() {
        let dir = fixture();
        write(dir.path(), "plan.md", b"# hi\n");
        write(dir.path(), "notes.txt", b"x");
        write(dir.path(), "paper.pdf", b"%PDF-1.4 fake");
        write(dir.path(), "sketch.rs", b"fn main() {}");
        write(dir.path(), "blob.png", b"\x89PNG fake");

        for probe in ["plan.md", "notes.txt", "sketch.rs"] {
            let cold = read(dir.path(), &[], probe, false, CEILING).expect("reads");
            assert!(cold.reviewable, "{probe}: reviewable however the reader got there");
        }
        for probe in ["paper.pdf", "blob.png"] {
            let read_only = read(dir.path(), &[], probe, false, CEILING).expect("reads");
            assert!(!read_only.reviewable, "{probe}: read-only whatever the caller asked");
        }
    }

    #[test]
    fn a_missing_document_is_refused_by_name() {
        let dir = fixture();

        let refusal =
            read(dir.path(), &[], "docs/gone.md", false, CEILING).expect_err("nothing to read");

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

        let refusal =
            read(dir.path(), &[], "big.txt", false, CEILING).expect_err("over the ceiling");

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
            let refusal = read(&cwd, &[], &probe, false, CEILING).expect_err("escapes never read");
            assert!(
                matches!(refusal, DocumentRefusal::OutsideTheTree { .. }),
                "{probe}: {refusal:?}"
            );
            assert!(refusal.to_string().contains(&probe), "the refusal names {probe}: {refusal}");
        }
    }

    /// A `..` walk whose target does not exist is still an ESCAPE, not a
    /// missing document: with no parent chain to canonicalize, the climb is
    /// decided lexically, so the refusal names the boundary rather than a
    /// file that was never there. A missing document INSIDE the tree keeps
    /// reading missing, parents absent or not - the control.
    #[test]
    fn an_escape_through_a_missing_parent_chain_refuses_as_outside_the_tree() {
        let dir = fixture();
        let root = dir.path().canonicalize().expect("canonical");
        let cwd = root.join("tree");
        std::fs::create_dir_all(&cwd).expect("the session tree");

        let refusal = read(&cwd, &[], "../../etc/hosts", false, CEILING)
            .expect_err("climbing out never reads, existing or not");
        assert!(
            matches!(refusal, DocumentRefusal::OutsideTheTree { .. }),
            "the boundary is the refusal: {refusal:?}",
        );

        let refusal = read(&cwd, &[], "gone/deeper/missing.md", false, CEILING)
            .expect_err("a missing document inside the tree still reads missing");
        assert!(matches!(refusal, DocumentRefusal::Missing { .. }), "{refusal:?}");
    }

    /// A file under a configured vault root reads exactly as an in-tree
    /// one does, and the root is handed in UNCANONICALIZED - on this
    /// machine `dir.path()` is the `/var` -> `/private/var` case - so
    /// the check canonicalizes both sides. An in-tree read with roots
    /// configured is the control: roots widen, they never narrow.
    #[test]
    fn a_document_under_a_vault_root_reads_as_an_in_tree_one() {
        let dir = fixture();
        let base = dir.path().canonicalize().expect("canonical");
        let cwd = base.join("tree");
        std::fs::create_dir_all(&cwd).expect("the session tree");
        write(&base, "vault/notes.md", b"# vault\n");
        write(&base, "tree/inside.md", b"# inside\n");
        let roots = vec![dir.path().join("vault")];

        let vaulted = base.join("vault/notes.md").to_str().expect("utf8").to_owned();
        let document = read(&cwd, &roots, &vaulted, false, CEILING).expect("a vault file reads");
        assert_eq!(document.kind, DocumentKind::Markdown, "the kind, as in-tree");
        assert_eq!(document.bytes, b"# vault\n");
        assert_eq!(document.path, base.join("vault/notes.md"), "resolved to its canonical path");

        let inside = read(&cwd, &roots, "inside.md", false, CEILING).expect("the tree reads");
        assert_eq!(inside.path, cwd.join("inside.md"), "the tree is not narrowed by the roots");
    }

    /// Outside the tree AND outside every root is the refusal it always
    /// was, in the same sentence: a `..` walk, an absolute path and a
    /// symlink pointing out are all refused by name with roots in play.
    #[test]
    fn a_path_outside_the_tree_and_every_root_still_refuses_by_name() {
        let dir = fixture();
        let base = dir.path().canonicalize().expect("canonical");
        let cwd = base.join("tree");
        std::fs::create_dir_all(&cwd).expect("the session tree");
        std::fs::create_dir_all(base.join("vault")).expect("a root that holds nothing for it");
        write(&base, "outside.md", b"secret");
        std::os::unix::fs::symlink(base.join("outside.md"), cwd.join("link.md"))
            .expect("the escaping symlink");
        let absolute = base.join("outside.md").to_str().expect("utf8").to_owned();
        let roots = vec![dir.path().join("vault")];

        for probe in ["../outside.md".to_owned(), absolute, "link.md".to_owned()] {
            let refusal =
                read(&cwd, &roots, &probe, false, CEILING).expect_err("escapes never read");
            assert!(
                matches!(refusal, DocumentRefusal::OutsideTheTree { .. }),
                "{probe}: {refusal:?}"
            );
            assert!(refusal.to_string().contains(&probe), "the refusal names {probe}: {refusal}");
        }
    }

    /// A symlink under a root that points out of every root refuses:
    /// the canonicalized target is what the check sees, not the link's
    /// place. The root's own real file is the control.
    #[test]
    fn a_symlink_out_of_a_root_refuses() {
        let dir = fixture();
        let base = dir.path().canonicalize().expect("canonical");
        let cwd = base.join("tree");
        std::fs::create_dir_all(&cwd).expect("the session tree");
        write(&base, "vault/real.md", b"# real\n");
        write(&base, "elsewhere/secret.md", b"secret");
        std::os::unix::fs::symlink(base.join("elsewhere/secret.md"), base.join("vault/link.md"))
            .expect("the escaping symlink");
        let roots = vec![base.join("vault")];

        let link = base.join("vault/link.md").to_str().expect("utf8").to_owned();
        let refusal = read(&cwd, &roots, &link, false, CEILING).expect_err("out of every root");
        assert!(matches!(refusal, DocumentRefusal::OutsideTheTree { .. }), "{refusal:?}");

        let real = base.join("vault/real.md").to_str().expect("utf8").to_owned();
        read(&cwd, &roots, &real, false, CEILING).expect("the root's own file still reads");
    }

    /// A sibling whose name starts with a root's is NOT inside it: the
    /// comparison is by path component, so `MadnessX` is not under a
    /// `Madness` root however their strings start.
    #[test]
    fn a_sibling_of_a_root_is_not_inside_it() {
        let dir = fixture();
        let base = dir.path().canonicalize().expect("canonical");
        let cwd = base.join("tree");
        std::fs::create_dir_all(&cwd).expect("the session tree");
        write(&base, "Madness/plan.md", b"# plan\n");
        write(&base, "MadnessX/plan.md", b"# plan\n");
        let roots = vec![base.join("Madness")];

        let root_file = base.join("Madness/plan.md").to_str().expect("utf8").to_owned();
        read(&cwd, &roots, &root_file, false, CEILING).expect("the root itself reads");

        let sibling = base.join("MadnessX/plan.md").to_str().expect("utf8").to_owned();
        let refusal =
            read(&cwd, &roots, &sibling, false, CEILING).expect_err("a sibling is not inside");
        assert!(matches!(refusal, DocumentRefusal::OutsideTheTree { .. }), "{refusal:?}");
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

        let refusal = read(dir.path(), &[], "locked.txt", false, CEILING).expect_err("locked");

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

        let document = read(dir.path(), &[], "paper.pdf", false, CEILING).expect("a PDF reads");
        assert_eq!(document.kind, DocumentKind::Pdf);
        assert_eq!(document.bytes, b"%PDF-1.4 fake");

        let refusal =
            read(dir.path(), &[], "paper.pdf", true, CEILING).expect_err("reviewable is refused");
        assert!(matches!(refusal, DocumentRefusal::PdfNotReviewable), "{refusal:?}");
        assert!(refusal.to_string().contains("read-only"), "names the reason: {refusal}");
    }

    #[test]
    fn a_zero_byte_document_reads_with_no_bytes() {
        let dir = fixture();
        write(dir.path(), "empty.md", b"");

        let document =
            read(dir.path(), &[], "empty.md", false, CEILING).expect("an empty file is a document");

        assert_eq!(document.kind, DocumentKind::Markdown);
        assert!(document.bytes.is_empty());
    }

    /// A text-bearing kind that is not UTF-8 is refused at the read rather
    /// than handed on: a viewer that decoded it would draw replacement
    /// characters, and the agent asked for a document it cannot show. An
    /// image kind is never checked - its bytes are drawn as the kind's own
    /// name, not as text.
    #[test]
    fn a_non_utf8_text_document_is_refused_and_other_kinds_are_not() {
        let dir = fixture();
        let bytes = [0xff, 0xfe, 0x00, 0x01];
        write(dir.path(), "broken.md", &bytes);
        write(dir.path(), "broken.txt", &bytes);
        write(dir.path(), "broken.rs", &bytes);
        write(dir.path(), "blob.png", &bytes);

        for probe in ["broken.md", "broken.txt", "broken.rs"] {
            let refusal = read(dir.path(), &[], probe, false, CEILING).expect_err("not text");
            assert!(matches!(refusal, DocumentRefusal::NotUtf8 { .. }), "{probe}: {refusal:?}");
            assert!(refusal.to_string().contains("UTF-8"), "{probe}: names the reason: {refusal}");
        }

        let other =
            read(dir.path(), &[], "blob.png", false, CEILING).expect("an image kind is not text");
        assert_eq!(other.kind, DocumentKind::Other("png".to_owned()));
    }
}
