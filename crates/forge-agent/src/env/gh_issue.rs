//! The issue mirror's process half: probe a repo, create an issue, list
//! what exists, close one.
//!
//! Same spawn hygiene as the PR lookup beside it (`git_command` scrubs
//! the git directory vars; a timeout kills the child): `gh` is invoked
//! from the project's own directory, and every failure is a named
//! outcome rather than an error the caller has to interpret.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use crate::env::git_command;

/// How long one `gh` call may take before it is killed.
const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// What the probe found about the repo `cwd` sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSupport {
    /// A github repo with issues enabled.
    Supported,
    /// A repo (or a directory) where issues are not a thing.
    Unsupported,
    /// No `gh` on PATH.
    GhMissing,
    /// `gh` ran and failed (auth, network, not a repo).
    Failed,
}

/// One issue as the mirror reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    pub number: u64,
    pub url: String,
    pub state: String,
}

/// One issue in a listing: number and state for the board's reconcile,
/// the body for the filing marker a gone row is found through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    pub number: u64,
    pub state: String,
    pub body: String,
}

/// Why the mirror closes an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The row completed, or its tree closed.
    Completed,
    /// The row is gone.
    NotPlanned,
}

impl CloseReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NotPlanned => "not planned",
        }
    }
}

async fn gh_output(cwd: &Path, args: &[&str]) -> Option<Output> {
    let mut command = git_command::tokio_command("gh");
    command.current_dir(cwd).args(args).kill_on_drop(true);
    match tokio::time::timeout(GH_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Some(output),
        Ok(Err(error)) => {
            tracing::debug!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_issue_spawn_failed",
                error = %error,
                args = ?args,
                "gh spawn failed",
            );
            None
        }
        Err(_) => {
            tracing::debug!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_issue_timeout",
                args = ?args,
                "gh timed out",
            );
            None
        }
    }
}

/// Whether the repo `cwd` sits in has issues enabled. Every non-answer -
/// no gh, not a repo, auth trouble - reads as something other than
/// `Supported`, so the mirror never files into a place that cannot take
/// it.
pub async fn probe_issues(cwd: &Path) -> IssueSupport {
    let Some(output) =
        gh_output(cwd, &["repo", "view", "--json", "hasIssuesEnabled", "-q", ".hasIssuesEnabled"])
            .await
    else {
        return IssueSupport::GhMissing;
    };
    if !output.status.success() {
        return IssueSupport::Failed;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => IssueSupport::Supported,
        _ => IssueSupport::Unsupported,
    }
}

/// File one issue and read back the number and url `gh` prints.
pub async fn create_issue(
    cwd: &Path,
    title: &str,
    body: &str,
    parent: Option<u64>,
) -> Option<IssueRef> {
    let parent_text = parent.map(|number| number.to_string());
    let mut args = vec!["issue", "create", "--title", title, "--body", body];
    if let Some(parent) = &parent_text {
        args.push("--parent");
        args.push(parent);
    }
    let output = gh_output(cwd, &args).await?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let number = url.rsplit('/').next()?.parse().ok()?;
    Some(IssueRef { number, url, state: "open".to_owned() })
}

/// Every issue with its state and body: the state so a close in GitHub
/// shows on the board, the body so an issue whose row is gone is found.
///
/// Open issues are read with their bodies - the marker is what the close
/// step matches - and closed ones without, the reconcile needing only
/// their state. Each read is a page walk to exhaustion under a ceiling
/// far above any tracker this runs against: `--limit` bounds the walk,
/// not the result, and a small cap silently hides the oldest rows from
/// both the reconcile and the close.
pub async fn list_issues(cwd: &Path) -> Vec<IssueRow> {
    let mut rows = gh_issues(cwd, "open", true).await;
    rows.extend(gh_issues(cwd, "closed", false).await);
    rows
}

async fn gh_issues(cwd: &Path, state: &str, bodies: bool) -> Vec<IssueRow> {
    const CEILING: &str = "5000";
    let fields = if bodies { "number,state,body" } else { "number,state" };
    let Some(output) =
        gh_output(cwd, &["issue", "list", "--state", state, "--limit", CEILING, "--json", fields])
            .await
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(parsed) => parsed,
        Err(_) => return Vec::new(),
    };
    parsed
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let number = row.get("number")?.as_u64()?;
                    let state = row.get("state")?.as_str()?.to_ascii_lowercase();
                    let body = row
                        .get("body")
                        .and_then(|body| body.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    Some(IssueRow { number, state, body })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Close one issue. Answers whether it closed: a failed close (already
/// closed, auth, network) is a non-event for the caller's next pass.
pub async fn close_issue(cwd: &Path, number: u64, reason: CloseReason) -> bool {
    let number = number.to_string();
    let Some(output) =
        gh_output(cwd, &["issue", "close", &number, "--reason", reason.as_str()]).await
    else {
        return false;
    };
    output.status.success()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI's own words, which are not the REST API's (`state_reason`
    /// spells it `not_planned`): a wrong token makes every close fail
    /// silently.
    #[test]
    fn close_reasons_are_the_cli_own_strings() {
        assert_eq!(CloseReason::Completed.as_str(), "completed");
        assert_eq!(CloseReason::NotPlanned.as_str(), "not planned");
    }
}
