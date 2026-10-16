//! One file's whole content at a revision, as its lines.
//!
//! The review page draws a diff's bounded hunks, so the lines BETWEEN them
//! are exactly what its record does not carry; expanding a gap needs the
//! file itself, read at the revision the scope's diff describes. `git show
//! <rev>:<path>` is that read - the revision is a commit's own tree for a
//! commit scope, and the branch tip for the all-changes one.
//!
//! The read is bounded by the caller's ceiling and says so with a flag
//! rather than a refusal: a file too long to carry whole still supplies the
//! gaps its carried lines reach, which is worth more to the reader than
//! nothing at all.

use std::path::Path;
use std::time::Duration;

use super::{COMMAND_TIMEOUT, GitOutput, run_git_within};

/// One file's content at a revision, bounded by the caller's ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLines {
    /// The file's lines, without their terminators, in file order.
    pub lines: Vec<String>,
    /// The ceiling cut the read short: what is below the last line was not
    /// carried.
    pub truncated: bool,
}

/// Why a file's content could not be read at a revision.
#[derive(Debug, thiserror::Error)]
pub enum FileReadRefusal {
    #[error("no file at {path} in {rev}")]
    Missing { rev: String, path: String },
    #[error("the file could not be read: {path} at {rev} ({why})")]
    Unreadable { rev: String, path: String, why: String },
}

/// Read `path`'s content in `rev`, as its lines, up to `ceiling_bytes`.
///
/// A path the revision does not carry is [`FileReadRefusal::Missing`] - a
/// deleted file's new side, for one - and a subprocess that could not run is
/// [`FileReadRefusal::Unreadable`]. The two are apart because a viewer draws
/// each as itself, the way the diff reader keeps a read that answered
/// something apart from one that could not run.
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
    let spec = format!("{rev}:{path}");
    let raw = match run_git_within(cwd, &["show", &spec], budget).await {
        GitOutput::Ok(raw) => raw,
        // `git show` exits non-zero for a path the revision does not carry,
        // which is the missing arm rather than a failure to read: an empty
        // file is the legitimate empty output.
        GitOutput::Empty => String::new(),
        GitOutput::Failed => {
            return Err(FileReadRefusal::Missing { rev: rev.to_owned(), path: path.to_owned() });
        }
        GitOutput::Oversize => {
            return Err(FileReadRefusal::Unreadable {
                rev: rev.to_owned(),
                path: path.to_owned(),
                why: "bigger than a single read may carry".to_owned(),
            });
        }
    };
    Ok(bounded_lines(&raw, ceiling_bytes))
}

/// The file's lines, cut at `ceiling_bytes` whole lines at a time: a line
/// the ceiling cannot hold is dropped rather than carried in part, so every
/// line the read answers is a line of the file.
fn bounded_lines(raw: &str, ceiling_bytes: u64) -> FileLines {
    let ceiling = usize::try_from(ceiling_bytes).unwrap_or(usize::MAX);
    let mut lines = Vec::new();
    let mut carried = 0_usize;
    let mut truncated = false;
    for line in raw.lines() {
        let next = carried + line.len() + 1;
        if next > ceiling {
            truncated = true;
            break;
        }
        lines.push(line.to_owned());
        carried = next;
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

    /// The read answers the file at the revision asked for, and a revision
    /// it does not carry is Missing rather than an empty answer - the two
    /// are different facts, and a viewer draws each as itself.
    #[tokio::test]
    async fn reads_a_files_lines_at_a_revision() {
        let dir = tempfile::tempdir().expect("a directory");
        repo(dir.path());
        let head = {
            let out = StdCommand::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };

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

    /// **The ceiling drops a whole line rather than half of one.** Every
    /// line the read answers is a line of the file, because the page draws
    /// them as context beside carried lines, where a partial line would
    /// read as content the file does not have.
    #[tokio::test]
    async fn the_ceiling_carries_whole_lines_and_says_what_it_dropped() {
        let raw = "alpha\nbeta\ngamma\n";
        // `alpha` + its terminator is 6 bytes; the second line does not fit.
        let cut = bounded_lines(raw, 6);
        assert_eq!(cut.lines, vec!["alpha"], "the first whole line only");
        assert!(cut.truncated, "and the read says it stopped short");

        let whole = bounded_lines(raw, u64::MAX);
        assert_eq!(whole.lines, vec!["alpha", "beta", "gamma"]);
        assert!(!whole.truncated);

        // An empty file is a legitimate empty answer, not a refusal.
        let empty = bounded_lines("", 1024);
        assert!(empty.lines.is_empty());
        assert!(!empty.truncated);
    }
}
