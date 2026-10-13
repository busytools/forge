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

use std::collections::HashMap;
use std::process::Command;

/// A `git` / `gh` spawn with the ambient repo-location env scrubbed,
/// for synchronous callers anywhere in the crate (the plugin
/// rollback's marketplace-clone git steps included).
pub(crate) fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_COMMON_DIR");
    command
}

/// The [`command`] variant async callers await; the scrub rides the
/// std -> tokio conversion.
pub(super) fn tokio_command(program: &str) -> tokio::process::Command {
    command(program).into()
}

/// The environment variables `gh` reads for auth, in its own precedence
/// order; GITHUB_PAT_TOKEN is not one of them, but the environment that
/// carries the hazard declares it beside the two that are.
const AMBIENT_IDENTITY_VARS: [&str; 3] = ["GH_TOKEN", "GITHUB_TOKEN", "GITHUB_PAT_TOKEN"];

/// A `gh` spawn for one project's own work: [`command`]'s scrub, the
/// ambient identity removed, then the project's declared `env` applied -
/// so a project that declares its own token acts as that project, and
/// one that declares none falls back to gh's own login rather than
/// silently to the server's environment.
pub(super) fn gh_tokio_command<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
) -> tokio::process::Command {
    let mut command = tokio_command("gh");
    for key in AMBIENT_IDENTITY_VARS {
        command.env_remove(key);
    }
    command.envs(env);
    command
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
        // Production gh spawns leave `command` for `gh_tokio_command`, so
        // the repo-location scrub is pinned on that path too.
        let gh_removed = tokio_removed_keys(&gh_tokio_command(&HashMap::new()));
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
            let scrubbed = gh_removed.iter().any(|k| k == key);
            assert!(scrubbed, "the gh spawn is missing the {key} scrub");
        }
    }

    /// The project's own identity reaches the child: a gh spawn built
    /// for a project that declares its own GH_TOKEN carries it, so that
    /// project's PR read answers as the project rather than as whoever
    /// the server's environment says.
    #[test]
    fn the_gh_constructor_carries_the_projects_own_identity() {
        let env = HashMap::from([("GH_TOKEN".to_owned(), "project-token".to_owned())]);
        let command = gh_tokio_command(&env);
        let carried = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "GH_TOKEN")
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str());
        assert_eq!(carried, Some("project-token"), "the project's GH_TOKEN is on the spawn");
    }

    /// And the server's identity never reaches it: a project that
    /// declares no token of its own gets gh's own login, not the
    /// environment forge itself was launched with. `gh` reads GH_TOKEN
    /// then GITHUB_TOKEN; GITHUB_PAT_TOKEN rides along because the
    /// environment that carries the hazard declares it beside them.
    #[test]
    fn the_gh_constructor_scrubs_the_ambient_identity_variables() {
        let removed = tokio_removed_keys(&gh_tokio_command(&HashMap::new()));
        for key in ["GH_TOKEN", "GITHUB_TOKEN", "GITHUB_PAT_TOKEN"] {
            assert!(
                removed.iter().any(|k| k == key),
                "{key} is not removed from a gh spawn: {removed:?}",
            );
        }
    }
}
