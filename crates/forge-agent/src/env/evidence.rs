//! The evidence gate's check: does the artifact a completion names exist
//! in the project's repository?
//!
//! forge verifies the claim itself, from the repository on disk, with no
//! network and no connector: a commit sha must be an object in the repo,
//! and a named path must exist at that commit (or at `HEAD` when the
//! claim names no commit). The excerpt a caller attaches is the reader's
//! evidence and is never what is checked.
//!
//! **Synchronous on purpose.** The check runs inside the task move that
//! claims the completion, which is a lock-taking write rather than an
//! async path; `git cat-file` against a local repository is a bounded
//! local read and takes no index lock, so it is safe to run there.

use std::path::Path;
use std::process::Command;

/// How long the git child may run before the check gives up. A local
/// `cat-file` answers in milliseconds; a pathological repository (an
/// unreadable pack, a hung filesystem) must not hold the board's lock.
const GIT_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A project's own check of a completion's artifact.
///
/// A trait so a test can drive the gate's refusals without building a
/// repository, and so the connector work can sit behind the same seam
/// later; the production implementation is [`GitEvidence`].
pub trait EvidenceVerifier: Send + Sync {
    /// `Ok(())` when every artifact the claim names exists; `Err(reason)`
    /// otherwise, with the reason written for the caller who has to fix
    /// it.
    fn verify(&self, repo: &Path, commit: Option<&str>, path: Option<&str>) -> Result<(), String>;
}

/// The self-contained verifier: `git cat-file` against the project's
/// repository.
pub struct GitEvidence;

impl EvidenceVerifier for GitEvidence {
    fn verify(&self, repo: &Path, commit: Option<&str>, path: Option<&str>) -> Result<(), String> {
        if commit.is_none() && path.is_none() {
            return Err(
                "the claim names no commit and no path, so there is nothing to check".into()
            );
        }
        if let Some(commit) = commit {
            exists(repo, &format!("{commit}^{{commit}}"))
                .map_err(|()| format!("commit {commit} does not exist in {}", repo.display()))?;
        }
        if let Some(path) = path {
            let at = commit.unwrap_or("HEAD");
            exists(repo, &format!("{at}:{path}")).map_err(|()| {
                format!("path {path} does not exist at {at} in {}", repo.display())
            })?;
        }
        Ok(())
    }
}

/// Whether `spec` names an object in the repository at `repo`, using the
/// same cat-file form `git cat-file -e` documents.
fn exists(repo: &Path, spec: &str) -> Result<(), ()> {
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .arg("cat-file")
        .arg("-e")
        .arg(spec)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| ())?;
    let deadline = std::time::Instant::now() + GIT_CHECK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return if status.success() { Ok(()) } else { Err(()) },
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(dir: &Path) -> PathBuf {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "forge")
                .env("GIT_AUTHOR_EMAIL", "forge@example.invalid")
                .env("GIT_COMMITTER_NAME", "forge")
                .env("GIT_COMMITTER_EMAIL", "forge@example.invalid")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("done.txt"), "the work\n").expect("write");
        std::fs::create_dir_all(dir.join("docs")).expect("mkdir");
        std::fs::write(dir.join("docs/report.md"), "the report\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "the work"]);
        dir.to_path_buf()
    }

    fn head(repo: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
    }

    /// The self-contained check: a commit in the repository, and a path at
    /// that commit or at HEAD.
    #[test]
    fn a_commit_and_a_path_are_checked_against_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = repo(dir.path());
        let head = head(&repo);
        let evidence = GitEvidence;

        evidence.verify(&repo, Some(&head), Some("done.txt")).expect("the commit and path exist");
        evidence.verify(&repo, None, Some("docs/report.md")).expect("the path exists at HEAD");
        evidence.verify(&repo, Some(&head), None).expect("the commit alone is a claim");
    }

    /// Absence is never success: a commit the repository does not hold and
    /// a path the commit does not carry are each refused, naming what was
    /// checked.
    #[test]
    fn an_absent_commit_or_path_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = repo(dir.path());
        let head = head(&repo);
        let evidence = GitEvidence;

        let refused =
            evidence.verify(&repo, Some("deadbeefdeadbeef"), None).expect_err("no commit");
        assert!(refused.contains("deadbeefdeadbeef"), "the refusal names the commit: {refused}");
        assert!(refused.contains(&repo.display().to_string()), "and the repository: {refused}");

        let refused = evidence
            .verify(&repo, Some(&head), Some("missing.txt"))
            .expect_err("no such path at the commit");
        assert!(refused.contains("missing.txt"), "the refusal names the path: {refused}");

        let refused = evidence.verify(&repo, None, None).expect_err("an empty claim");
        assert!(refused.contains("nothing to check"), "{refused}");
    }

    /// A directory that is not a repository cannot verify anything.
    #[test]
    fn a_non_repository_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refused =
            GitEvidence.verify(dir.path(), None, Some("anything")).expect_err("not a repo");
        assert!(refused.contains("anything"), "{refused}");
    }
}
