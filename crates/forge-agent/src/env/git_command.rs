//! The shared constructor every production `git` / `gh` spawn goes
//! through.
//!
//! `GIT_DIR` / `GIT_WORK_TREE` / `GIT_COMMON_DIR` override `-C` and
//! `current_dir` outright, so without the scrub a forge launched from
//! a git hook, a `git rebase --exec`, or any shell that exported them
//! answers every question about every repo from one foreign repo -
//! at exit 0, with nothing in the output to say so. Positioning the
//! process (`-C` for git, `current_dir` for gh) and args stay with
//! the call site; nothing there may set a repo-location variable.

use std::process::Command;

/// A `git` / `gh` spawn with the ambient repo-location env scrubbed,
/// for synchronous callers.
pub(super) fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_COMMON_DIR");
    command
}

/// The [`command`] variant async callers await; the scrub rides the
/// std -> tokio conversion.
pub(super) fn tokio_command(program: &str) -> tokio::process::Command {
    command(program).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn removed_keys(command: &Command) -> Vec<String> {
        command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str().map(str::to_owned))
            .collect()
    }

    fn tokio_removed_keys(command: &tokio::process::Command) -> Vec<String> {
        // The conversion keeps the inner std Command verbatim, so the
        // scrub is readable through `as_std`.
        removed_keys(command.as_std())
    }

    /// One constructor, so one pin covers every production git/gh
    /// spawn: deleting any `env_remove` here fails this test, and no
    /// call site can lose the scrub without leaving this function.
    #[test]
    fn the_shared_constructor_scrubs_every_repo_location_variable() {
        for program in ["git", "gh"] {
            let std_removed = removed_keys(&command(program));
            let tokio_removed = tokio_removed_keys(&tokio_command(program));
            for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
                assert!(
                    std_removed.iter().any(|k| k == key),
                    "{program} (std) is missing the {key} scrub",
                );
                assert!(
                    tokio_removed.iter().any(|k| k == key),
                    "{program} (tokio) is missing the {key} scrub",
                );
            }
        }
    }
}
