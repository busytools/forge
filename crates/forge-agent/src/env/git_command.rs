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

/// Whether forge builds the ssh command for a child, or the checkout does.
///
/// **`GIT_SSH_COMMAND` outranks both `core.sshCommand` and `GIT_SSH`**, so a
/// command forge sets is the one that runs: it may not be set while either of
/// the others is the user's own way to reach their remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SshCommand {
    /// The environment and the checkout name none, so forge's bounded one
    /// runs.
    Forge,
    /// One of the user's spellings is in force, and it is left alone.
    User,
}

/// A `git` child that may reach a remote.
///
/// **No prompt path, and no unbounded one.** A child on a dead link ends in an
/// error rather than holding a read open for the process's life: git's
/// terminal prompt is off, the environment's askpass hooks are removed, http
/// aborts below a trickle, and when the checkout names no ssh command of its
/// own, forge's bounds ssh's connect and keeps the session alive. `core.askPass`
/// is a checkout's own configuration and is not touched: it is a helper that
/// answers or fails, not a terminal prompt.
///
/// This is the network children's alone, not the shared constructor's (#2668):
/// a local scan has nothing to prompt for, and a plugin's clone still belongs
/// to whoever asked for it.
pub(super) fn network_tokio_git_command(
    cwd: &std::path::Path,
    ssh: SshCommand,
) -> tokio::process::Command {
    let mut command = tokio_git_command(cwd);
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env_remove("GIT_ASKPASS").env_remove("SSH_ASKPASS");
    command.args(["-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=20"]);
    if ssh == SshCommand::Forge {
        command.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=10 \
             -o ServerAliveCountMax=3",
        );
    }
    command
}

/// The [`command`] variant async callers await; the scrub rides the
/// std -> tokio conversion.
pub(super) fn tokio_command(program: &str) -> tokio::process::Command {
    command(program).into()
}

/// A `git` spawn positioned at `cwd`.
///
/// **The machine's own `git`, unless a test pointed that directory
/// elsewhere** (`point_git_at`): a warm `git` can run to completion
/// before a caller's first poll, so a case that needs a child which
/// cannot - a zero-budget refusal - has to drive a wrapper of its own.
pub(super) fn tokio_git_command(cwd: &std::path::Path) -> tokio::process::Command {
    #[cfg(test)]
    if let Some(program) = test_git_program(cwd) {
        return tokio_command(&program.to_string_lossy());
    }
    #[cfg(not(test))]
    let _ = cwd;
    tokio_command("git")
}

/// Point every `git` spawn positioned at `cwd` at `program`, for this
/// process's tests. Keyed by directory so parallel cases never run the
/// machine's `git` through another case's wrapper.
#[cfg(test)]
pub(super) fn point_git_at(cwd: &std::path::Path, program: &std::path::Path) {
    git_programs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((cwd.to_path_buf(), program.to_path_buf()));
}

/// Drop a directory's pointer, back to the machine's `git`.
#[cfg(test)]
pub(super) fn clear_git_point(cwd: &std::path::Path) {
    git_programs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(pointed, _)| pointed != cwd);
}

#[cfg(test)]
fn test_git_program(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    git_programs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .rev()
        .find(|(pointed, _)| pointed == cwd)
        .map(|(_, program)| program.clone())
}

#[cfg(test)]
fn git_programs() -> &'static std::sync::Mutex<Vec<(std::path::PathBuf, std::path::PathBuf)>> {
    static POINTS: std::sync::OnceLock<
        std::sync::Mutex<Vec<(std::path::PathBuf, std::path::PathBuf)>>,
    > = std::sync::OnceLock::new();
    POINTS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
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

    /// **A network git child can never wait on a person, nor on a dead link.**
    /// The review page's read waits for its base fetch to finish, so a prompt
    /// inherited from the shell forge was launched in would hold the page open
    /// for as long as nobody answers it, and a blackholed remote would hold it
    /// open for ever. Every such child ends in a git error instead: the
    /// terminal prompt is off, the environment's askpass hooks are removed,
    /// http aborts below a trickle, and ssh is bounded when the checkout names
    /// no command of its own.
    #[test]
    fn a_network_git_child_can_wait_on_neither_a_person_nor_a_dead_link() {
        let cwd = std::path::Path::new("/tmp/nowhere");
        let ours = network_tokio_git_command(cwd, SshCommand::Forge).into_std();
        let set = |command: &Command, key: &str| {
            command
                .get_envs()
                .find(|(k, _)| *k == key)
                .and_then(|(_, value)| value)
                .and_then(|value| value.to_str())
                .map(str::to_owned)
        };
        assert_eq!(
            set(&ours, "GIT_TERMINAL_PROMPT").as_deref(),
            Some("0"),
            "git's terminal prompt is disabled whatever the shell exported",
        );
        for key in ["GIT_ASKPASS", "SSH_ASKPASS"] {
            assert!(
                removed_keys(&ours).iter().any(|k| k == key),
                "{key} reaches the child from this process's environment",
            );
        }
        let args: Vec<String> =
            ours.get_args().filter_map(|arg| arg.to_str().map(str::to_owned)).collect();
        for pinned in ["http.lowSpeedLimit=1000", "http.lowSpeedTime=20"] {
            assert!(args.iter().any(|arg| arg == pinned), "http is bounded: {args:?}");
        }
        let ssh = set(&ours, "GIT_SSH_COMMAND").unwrap_or_default();
        for pinned in ["BatchMode=yes", "ConnectTimeout=15", "ServerAliveInterval=10"] {
            assert!(ssh.contains(pinned), "forge's ssh is bounded: {ssh:?}");
        }

        // **A checkout's own command is left alone**, whichever spelling holds
        // it: GIT_SSH_COMMAND outranks both, so forge must not set it.
        let theirs = network_tokio_git_command(cwd, SshCommand::User).into_std();
        assert!(
            set(&theirs, "GIT_SSH_COMMAND").is_none(),
            "the user's own ssh command is not overridden",
        );

        // And the shared constructor carries none of it: this is the network
        // children's, so a local scan and a plugin's clone are untouched.
        let local = command("git");
        assert!(set(&local, "GIT_TERMINAL_PROMPT").is_none(), "a local git is not reshaped");
        assert!(
            !removed_keys(&local).iter().any(|k| k == "GIT_ASKPASS"),
            "a local git keeps the environment's askpass hooks",
        );
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
