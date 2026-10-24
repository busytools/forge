//! The working tree as a handoff brief reads it: branch, `HEAD`, the dirty
//! paths and the diff's stat.
//!
//! Three probes rather than the diff scanner, which walks every changed file
//! and may ask GitHub about a pull request - a brief is written in the moment
//! a session swaps and cannot wait on either.

use std::path::Path;
use std::time::Duration;

use forge_primitives::HandoffGitState;

use super::git_command;

/// Each probe's own budget. A wedged git costs the brief that probe and no
/// more; the swap is already under way when this runs.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many dirty paths and diff lines the brief keeps. The brief is capped
/// by the data it names, so the read is capped here rather than by a
/// consumer.
const DIRTY_CAP: usize = 20;
const DIFF_CAP: usize = 12;

/// Read the three facts. A directory that is not a repository answers with
/// everything absent, which is a fact the brief states rather than an error.
pub async fn probe(cwd: &Path) -> HandoffGitState {
    let branch = run(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let head = run(cwd, &["rev-parse", "--short", "HEAD"]).await;
    // One probe says whether this is a repository at all: a directory that
    // is not one answers nothing to all four, and the brief draws that.
    let Some(branch) = branch else {
        return HandoffGitState::default();
    };
    let dirty = run(cwd, &["status", "--porcelain"])
        .await
        .map(|out| capped_lines(&out, DIRTY_CAP))
        .unwrap_or_default();
    let diff = run(cwd, &["diff", "--stat"])
        .await
        .map(|out| capped_lines(&out, DIFF_CAP))
        .unwrap_or_default();
    HandoffGitState {
        branch: Some(branch.trim().to_owned()),
        head: head.map(|out| out.trim().to_owned()),
        dirty,
        diff,
    }
}

/// One git line of output, or `None` when it failed, timed out or was empty.
async fn run(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut command = git_command::tokio_git_command(cwd);
    command.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

/// The last `cap` non-blank lines, which is the part of a long list that
/// says what the successor will meet.
fn capped_lines(output: &str, cap: usize) -> Vec<String> {
    let lines: Vec<String> = output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let keep = lines.len().saturating_sub(cap);
    lines.into_iter().skip(keep).collect()
}

#[cfg(test)]
mod tests {
    use super::capped_lines;

    /// A long list keeps its newest end, which is where the work in flight
    /// is; the cap is a property of the brief rather than of git.
    #[test]
    fn a_long_list_keeps_its_newest_lines() {
        let output = (0..30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let kept = capped_lines(&output, 12);
        assert_eq!(kept.len(), 12, "the cap is the cap");
        assert_eq!(kept.first().map(String::as_str), Some("line 18"), "and it is the newest end");
        assert_eq!(kept.last().map(String::as_str), Some("line 29"), "through the last line");
    }

    /// Blank lines are not rows a brief draws, and a list shorter than the
    /// cap is carried whole.
    #[test]
    fn blank_lines_are_dropped_and_a_short_list_is_whole() {
        let kept = capped_lines("a\n\n  \nb\n", 5);
        assert_eq!(kept, vec!["a".to_owned(), "b".to_owned()]);
    }
}
