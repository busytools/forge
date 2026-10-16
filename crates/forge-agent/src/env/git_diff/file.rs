//! One file's whole content at a revision, as its lines.
//!
//! The review page draws a diff's bounded hunks, so the lines BETWEEN them
//! are exactly what its record does not carry; expanding a gap needs the
//! file itself, read at the revision the scope's diff describes. `git show
//! <rev>:<path>` is that read - the revision is a commit's own tree for a
//! commit scope, and the branch tip for the all-changes one.
//!
//! **The read is bounded by the caller's ceiling and never by the file.**
//! Only a blob is a file (a gitlink's size is the commit line `git show`
//! prints for it), the size comes next (`cat-file -s`, a handful of bytes),
//! and the content is read through a child killed at the ceiling - so a
//! multi-gigabyte blob costs the ceiling's allocation and not its own, and
//! the content never goes through the layer's own subprocess bound
//! (`STDOUT_SIZE_CAP`, below the shipped ceiling), which is what keeps a
//! file between that bound and the ceiling answering a flagged short read
//! rather than a refusal. The LINES are bounded too ([`MAX_READ_LINES`]):
//! a file of very short lines meets that long before either byte bound, and
//! the answer's own JSON is the other half of what a read costs.
//!
//! **A revision off the wire is resolved before it reaches git.** `git show`
//! reads a `--output=<file>`-shaped revision as an option of its own rather
//! than as a revision, so an unvalidated one writes files wherever the
//! caller says; [`resolve_commit`] is the one gate, and what comes back is a
//! commit id - hex, never a dash - which every argv below interpolates.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use super::{COMMAND_TIMEOUT, GitOutput, run_git_within};

/// The most lines one read carries, whatever ceiling the caller hands in: a
/// file of very short lines meets this long before the byte ceiling, and
/// every line past it is one the answer's own JSON would pay for. The
/// response is the smaller of the two bounds, flagged whenever either bites.
pub const MAX_READ_LINES: usize = 10_000;

/// One file's content at a revision, bounded by the caller's ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLines {
    /// The file's lines, without their terminators, in file order.
    pub lines: Vec<String>,
    /// The ceiling or the line cap cut the read short: what is below the
    /// last line was not carried.
    pub truncated: bool,
}

/// Why a file's content could not be read at a revision, per child:
///
/// - The revision gate answers [`Self::Missing`] when `rev` names no commit.
/// - The type and size probes answer [`Self::Missing`] when their child
///   fails - git refuses a revision or a path it does not carry the same way
///   it refuses a protocol problem, and neither probe can tell those apart -
///   and when the path is not a BLOB (a gitlink or a tree is not a file, and
///   `git show` formats one as something whose size is not its content's).
/// - The content read answers [`Self::Unreadable`] when its child could not
///   start, the budget cut it off, or it exited non-zero.
#[derive(Debug, thiserror::Error)]
pub enum FileReadRefusal {
    #[error("no file at {path} in {rev}")]
    Missing { rev: String, path: String },
    #[error("the file could not be read: {path} at {rev} ({why})")]
    Unreadable { rev: String, path: String, why: String },
}

/// The commit `rev` names, as its full id, or `None` when it names nothing
/// git can read.
///
/// **This is where an untrusted revision is made safe.** `--end-of-options`
/// stops git reading an option-shaped string as an option of its own, and
/// the answer is checked to be a commit id before anything else uses it -
/// so every argv built from it is a revision and cannot be a flag.
pub async fn resolve_commit(cwd: &Path, rev: &str, budget: Duration) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    match run_git_within(cwd, &["rev-parse", "--verify", "--end-of-options", &spec], budget).await {
        GitOutput::Ok(raw) => {
            let id = raw.trim();
            // A full id, and nothing else: an option-shaped or otherwise
            // unusual answer is not a commit to interpolate into an argv.
            (id.len() >= 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| id.to_owned())
        }
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => None,
    }
}

/// Read `path`'s content in `rev`, as its lines, up to `ceiling_bytes`.
pub async fn read_file_lines(
    cwd: &Path,
    rev: &str,
    path: &str,
    ceiling_bytes: u64,
) -> Result<FileLines, FileReadRefusal> {
    read_file_lines_within(cwd, rev, path, ceiling_bytes, COMMAND_TIMEOUT).await
}

/// [`read_file_lines`] with its command budget spelled; see the module's
/// sibling readers for why the budget bounds the machine, not git.
pub async fn read_file_lines_within(
    cwd: &Path,
    rev: &str,
    path: &str,
    ceiling_bytes: u64,
    budget: Duration,
) -> Result<FileLines, FileReadRefusal> {
    let Some(commit) = resolve_commit(cwd, rev, budget).await else {
        return Err(FileReadRefusal::Missing { rev: rev.to_owned(), path: path.to_owned() });
    };
    let spec = format!("{commit}:{path}");
    let ceiling = usize::try_from(ceiling_bytes).unwrap_or(usize::MAX);
    // **Only a blob is a file.** A gitlink or a tree at the path answers a
    // size that is not its content's - a gitlink's is the commit line git
    // prints for it, a tree's is the listing's - so the type is what says
    // whether a size, and a read, mean anything here.
    if !is_blob(cwd, &spec, budget).await {
        return Err(FileReadRefusal::Missing { rev: rev.to_owned(), path: path.to_owned() });
    }
    // **The size is known before a line is read**, so a file the ceiling
    // cannot hold is never materialised to be refused.
    let Some(size) = file_size(cwd, &spec, budget).await else {
        return Err(FileReadRefusal::Missing { rev: rev.to_owned(), path: path.to_owned() });
    };
    // **The content never goes through the layer's own reader.** It carries
    // an output bound of its own ([`super::STDOUT_SIZE_CAP`], below the
    // shipped ceiling), and a file between the two would answer that bound's
    // refusal rather than the file; this read takes at most the ceiling and
    // nothing else.
    let Some(capped) = show_capped(cwd, &spec, ceiling, budget).await else {
        return Err(FileReadRefusal::Unreadable {
            rev: rev.to_owned(),
            path: path.to_owned(),
            why: "the read could not be taken".to_owned(),
        });
    };
    // The probe is what makes `truncated` certain rather than a guess: a
    // file inside the ceiling is answered whole, whatever the read's take
    // was bounded by.
    if size <= ceiling_bytes {
        return Ok(capped_lines(&String::from_utf8_lossy(&capped), false));
    }
    Ok(capped_lines(&String::from_utf8_lossy(&capped), true))
}

/// Whether the object at `spec` is a blob - the only kind this read calls a
/// file.
async fn is_blob(cwd: &Path, spec: &str, budget: Duration) -> bool {
    match run_git_within(cwd, &["cat-file", "-t", "--end-of-options", spec], budget).await {
        GitOutput::Ok(raw) => raw.trim() == "blob",
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => false,
    }
}

/// The blob's size in bytes, or `None` when git cannot read it.
async fn file_size(cwd: &Path, spec: &str, budget: Duration) -> Option<u64> {
    match run_git_within(cwd, &["cat-file", "-s", "--end-of-options", spec], budget).await {
        GitOutput::Ok(raw) => raw.trim().parse().ok(),
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => None,
    }
}

/// `git show <spec>`, at most `ceiling` bytes of its stdout and never a byte
/// more: the child is killed the moment the ceiling is reached, so what this
/// allocates is the ceiling's, not the file's. `None` when the child could
/// not start, the budget cut the read off, or it ended in failure - a `git
/// show` that failed after the size probe answered is a read that did not
/// happen, never an empty file.
async fn show_capped(cwd: &Path, spec: &str, ceiling: usize, budget: Duration) -> Option<Vec<u8>> {
    let mut command = super::super::git_command::tokio_git_command(cwd);
    command
        .arg("-C")
        .arg(cwd)
        .args(["show", "--end-of-options", spec])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let read = async {
        let mut child = command.spawn().ok()?;
        let out = child.stdout.take()?;
        let mut carried = Vec::with_capacity(ceiling.min(64 * 1024));
        out.take(ceiling as u64).read_to_end(&mut carried).await.ok()?;
        // A read that ended before the take did is the child's own end, so
        // its exit is the read's verdict; one that filled the take was cut
        // off on purpose and the child is killed there.
        if carried.len() < ceiling {
            let status = child.wait().await.ok()?;
            if !status.success() {
                return None;
            }
        } else {
            let _ = child.kill().await;
        }
        Some(carried)
    };
    tokio::time::timeout(budget, read).await.ok().flatten()
}

/// The file's lines, cut to the counts a frame can afford.
///
/// **Two bounds, and a frame pays for both.** The byte ceiling bounds the
/// content, and this bounds the LINES: a file of very short lines reaches
/// ten thousand long before a byte ceiling, and ten million `String`s is a
/// server-side allocation and a JSON answer nobody asked for. A line the
/// byte bound cuts in half is dropped rather than carried in part, so every
/// line answered is a line of the file.
fn capped_lines(raw: &str, cut_by_bytes: bool) -> FileLines {
    let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
    let mut truncated = cut_by_bytes;
    if truncated && !raw.ends_with('\n') {
        lines.pop();
    }
    if lines.len() > MAX_READ_LINES {
        lines.truncate(MAX_READ_LINES);
        truncated = true;
    }
    FileLines { lines, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::git_diff::TEST_GIT_BUDGET;
    use std::process::Command as StdCommand;

    fn repo(path: &Path) {
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(path).expect("the fixture's directory");
        git(&["init", "-q"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&["config", "user.email", "fixture@example.test"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(path.join("small.txt"), "alpha\nbeta\ngamma\n").expect("write");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
    }

    fn head_of(path: &Path) -> String {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// The read answers the file at the revision asked for, and a revision
    /// it does not carry is Missing rather than an empty answer.
    #[tokio::test]
    async fn reads_a_files_lines_at_a_revision() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let head = head_of(dir.path());

        let read =
            read_file_lines_within(dir.path(), &head, "small.txt", u64::MAX, TEST_GIT_BUDGET)
                .await
                .expect("the file reads");
        assert_eq!(read.lines, vec!["alpha", "beta", "gamma"], "the lines, in file order");
        assert!(!read.truncated, "and nothing withheld");

        let missing =
            read_file_lines_within(dir.path(), &head, "never.txt", u64::MAX, TEST_GIT_BUDGET).await;
        assert!(
            matches!(missing, Err(FileReadRefusal::Missing { .. })),
            "a path the revision does not carry is missing, not empty: {missing:?}",
        );
    }

    /// **A revision off the wire is never handed to git as an option.**
    /// `git show --output=<file>:path` writes the blob to `<file>` and runs
    /// at exit 0, so an unvalidated revision is a file write wherever the
    /// caller points it. The resolution is the gate; this is the pin, and
    /// the same shape is refused by the commit scan beside it.
    #[tokio::test]
    async fn an_option_shaped_revision_writes_nothing() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let target = dir.path().join("written-by-git");
        let shaped = format!("--output={}", target.display());

        let read =
            read_file_lines_within(dir.path(), &shaped, "small.txt", u64::MAX, TEST_GIT_BUDGET)
                .await;
        assert!(
            matches!(read, Err(FileReadRefusal::Missing { .. })),
            "an option-shaped revision resolves to no commit: {read:?}",
        );
        assert!(!target.exists(), "and git was never asked to write a file");
        assert!(
            resolve_commit(dir.path(), &shaped, TEST_GIT_BUDGET).await.is_none(),
            "the resolver refuses it outright",
        );
    }

    /// **The ceiling answers a short read rather than a refusal.** The size
    /// is known first, so a file the ceiling cannot hold is read to the
    /// ceiling and flagged - a line the ceiling cuts in half is dropped, so
    /// every line answered is a line of the file.
    #[tokio::test]
    async fn the_ceiling_carries_whole_lines_and_says_what_it_dropped() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let head = head_of(dir.path());

        // `alpha\n` is six bytes: the second line does not fit.
        let cut = read_file_lines_within(dir.path(), &head, "small.txt", 6, TEST_GIT_BUDGET)
            .await
            .expect("the read answers");
        assert_eq!(cut.lines, vec!["alpha"], "the whole lines the ceiling reaches");
        assert!(cut.truncated, "and the read says it stopped short");

        let whole = read_file_lines_within(dir.path(), &head, "small.txt", 18, TEST_GIT_BUDGET)
            .await
            .expect("the read answers");
        assert_eq!(whole.lines, vec!["alpha", "beta", "gamma"], "at its own size: everything");
        assert!(!whole.truncated, "and nothing withheld");

        // An empty file is a legitimate empty answer, not a refusal.
        std::fs::write(dir.path().join("empty.txt"), "").expect("write");
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "empty.txt"])
            .output()
            .expect("add");
        assert!(out.status.success());
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "empty"])
            .output()
            .expect("commit");
        assert!(out.status.success());
        let empty_head = head_of(dir.path());
        let empty =
            read_file_lines_within(dir.path(), &empty_head, "empty.txt", u64::MAX, TEST_GIT_BUDGET)
                .await
                .expect("an empty file reads");
        assert!(empty.lines.is_empty());
        assert!(!empty.truncated);
    }

    /// **A file between the git layer's subprocess bound and the caller's
    /// ceiling reads whole.** The layer refuses an output past
    /// [`super::STDOUT_SIZE_CAP`], and the shipped ceiling
    /// (`[documents] max_mib`) sits above it - so a file in between is
    /// exactly the case a read built on that layer answers a refusal for
    /// while the design promises the file, and it is why the content read
    /// does not go through the layer at all.
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_past_the_git_layers_own_bound_still_reads_whole() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let over = super::super::STDOUT_SIZE_CAP + 4096;
        // Long lines on purpose: the fixture must sit over the layer's bound
        // WITHOUT meeting the line cap, which bounds the answer's other half.
        let line = format!("{}\n", "x".repeat(899));
        let mut big = String::with_capacity(over + 1024);
        while big.len() < over {
            big.push_str(&line);
        }
        std::fs::write(dir.path().join("huge.txt"), &big).expect("write");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success());
        };
        git(&["add", "huge.txt"]);
        git(&["commit", "-qm", "huge"]);

        let head = head_of(dir.path());
        // The shipped ceiling, spelled: `[documents] max_mib` defaults to 10.
        let read = read_file_lines_within(
            dir.path(),
            &head,
            "huge.txt",
            10 * 1024 * 1024,
            TEST_GIT_BUDGET,
        )
        .await
        .expect("a file inside the ceiling reads");
        assert!(!read.truncated, "and nothing is withheld from it");
        assert_eq!(
            read.lines.iter().map(|line| line.len() + 1).sum::<usize>(),
            big.len(),
            "every line of it, whole",
        );
    }

    /// **Only a blob is a file.** A tree or a gitlink at the path answers a
    /// size that is not its content's (`git show` prints a listing, or the
    /// commit line, for them), so a read that trusted the size would draw
    /// lines no file has.
    #[tokio::test(flavor = "current_thread")]
    async fn a_path_that_is_not_a_blob_is_missing() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        std::fs::create_dir_all(dir.path().join("dir")).expect("mkdir");
        std::fs::write(dir.path().join("dir/inner.txt"), "x\n").expect("write");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success());
        };
        git(&["add", "-A"]);
        git(&["commit", "-qm", "dir"]);

        let head = head_of(dir.path());
        let tree =
            read_file_lines_within(dir.path(), &head, "dir", u64::MAX, TEST_GIT_BUDGET).await;
        assert!(
            matches!(tree, Err(FileReadRefusal::Missing { .. })),
            "a tree is not a file: {tree:?}",
        );
    }

    /// **A child that failed is a read that did not happen**, never an empty
    /// file: a `git show` that exits non-zero after the probes answered must
    /// not be drawn as a file of no lines.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_show_is_no_read_at_all() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let head = head_of(dir.path());

        let missing = format!("{head}:never.txt");
        assert!(
            show_capped(dir.path(), &missing, 4096, TEST_GIT_BUDGET).await.is_none(),
            "a failing child answers no content",
        );
        let there = format!("{head}:small.txt");
        assert!(
            show_capped(dir.path(), &there, 4096, TEST_GIT_BUDGET).await.is_some(),
            "and one that reads answers its bytes",
        );
    }

    /// **The LINES are bounded too.** A file of very short lines meets the
    /// line cap long before any byte ceiling, and the answer's own JSON is
    /// what that cap is for: ten thousand lines is the most one read
    /// carries, flagged.
    #[tokio::test(flavor = "current_thread")]
    async fn the_line_cap_holds_a_short_line_file() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let mut many = String::new();
        for _ in 0..(MAX_READ_LINES + 500) {
            many.push_str("l\n");
        }
        std::fs::write(dir.path().join("many.txt"), many).expect("write");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success());
        };
        git(&["add", "-A"]);
        git(&["commit", "-qm", "many"]);

        let head = head_of(dir.path());
        let read = read_file_lines_within(dir.path(), &head, "many.txt", u64::MAX, TEST_GIT_BUDGET)
            .await
            .expect("the read answers");
        assert_eq!(read.lines.len(), MAX_READ_LINES, "the cap's own count");
        assert!(read.truncated, "and the read says it stopped");
    }

    /// The capped read stops at the ceiling, whatever the file's size: the
    /// allocation is the harness's, so this holds at any size the fixture
    /// can write.
    #[tokio::test]
    async fn the_capped_read_never_carries_more_than_the_ceiling() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let mut big = String::new();
        for _ in 0..2000 {
            big.push_str("line\n");
        }
        std::fs::write(dir.path().join("big.txt"), big).expect("write");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success());
        };
        git(&["add", "big.txt"]);
        git(&["commit", "-qm", "big"]);

        let head = head_of(dir.path());
        let read = read_file_lines_within(dir.path(), &head, "big.txt", 64, TEST_GIT_BUDGET)
            .await
            .expect("the read answers");
        assert!(read.truncated, "a file past the ceiling answers a short read");
        assert!(
            read.lines.iter().map(|line| line.len() + 1).sum::<usize>() <= 64,
            "and every line it carries is inside the ceiling: {:?}",
            read.lines,
        );
        assert!(read.lines.len() > 1, "with the whole lines the ceiling reaches");
    }
}
