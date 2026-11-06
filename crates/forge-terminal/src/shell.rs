//! The shell integration scripts, and how a shell is started with them.
//!
//! **The person's own rc is never replaced.** zsh gets a `ZDOTDIR` shim that
//! sources the real `.zshenv`, `.zprofile` and `.zshrc` and then the
//! integration; bash gets an `--rcfile` that sources `~/.bashrc` first; fish
//! gets its init command. A shell forge does not know starts with no
//! integration, and the classifier says `heuristic`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The zsh integration: `precmd` closes the previous command and opens the
/// prompt, `preexec` opens the command.
pub const ZSH: &str = r#"
_forge_cmd_running=0
_forge_precmd() {
  local ret=$?
  if (( _forge_cmd_running )); then
    printf '\033]133;D;%s\007' "$ret"
    _forge_cmd_running=0
  fi
  printf '\033]133;A\007\033]133;B\007'
}
_forge_preexec() {
  _forge_cmd_running=1
  printf '\033]133;C\007'
}
precmd_functions+=(_forge_precmd)
preexec_functions+=(_forge_preexec)
"#;

/// The bash integration: `PROMPT_COMMAND` closes the previous command and
/// opens the prompt, a `DEBUG` trap opens the command.
///
/// Bash 3.2 (macOS) has no `PS0`, so the trap is the only pre-exec hook, and
/// it fires for the prompt's own commands too. Two guards keep the marks
/// honest: the trap hands `$?` back so `_forge_prompt` reads the command's
/// real status, and `_forge_in_prompt` plus the `$BASH_COMMAND` match keep the
/// prompt's commands from being announced as the person's.
pub const BASH: &str = r#"
_forge_cmd_running=
_forge_in_prompt=
_forge_status=0
_forge_prompt() {
  local ret=$_forge_status
  if [ -n "$_forge_cmd_running" ]; then
    printf '\033]133;D;%s\007' "$ret"
    _forge_cmd_running=
  fi
  printf '\033]133;A\007\033]133;B\007'
  _forge_in_prompt=1
}
_forge_preexec() {
  _forge_status=$?
  if [ -n "$_forge_in_prompt" ]; then
    return 0
  fi
  case "$BASH_COMMAND" in
    _forge_prompt) return 0 ;;
  esac
  if [ -z "$_forge_cmd_running" ]; then
    _forge_cmd_running=1
    printf '\033]133;C\007'
  fi
  return 0
}
PROMPT_COMMAND="_forge_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND};_forge_in_prompt="
trap '_forge_preexec' DEBUG
"#;

/// The fish integration: the `fish_prompt` and `fish_preexec` events.
pub const FISH: &str = r"
function _forge_prompt --on-event fish_prompt
  set -l ret $status
  if set -q _forge_cmd_running
    printf '\033]133;D;%s\007' $ret
    set -e _forge_cmd_running
  end
  printf '\033]133;A\007\033]133;B\007'
end
function _forge_preexec --on-event fish_preexec
  set -g _forge_cmd_running 1
  printf '\033]133;C\007'
end
";

/// How one shell is started, and the shim directory it needs.
#[derive(Debug)]
pub struct ShellPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// The shim directory, removed when the terminal drops.
    pub shim: Option<PathBuf>,
}

impl ShellPlan {
    /// Remove the shim directory, if one was made.
    pub fn cleanup(&self) {
        if let Some(shim) = &self.shim {
            let _ = std::fs::remove_dir_all(shim);
        }
    }
}

/// Build the command for `shell`, with the integration its name supports.
///
/// `env` is the seat's environment; the plan adds only what the integration
/// needs. A shell whose name is not zsh, bash or fish starts with no
/// integration.
pub fn plan<S: std::hash::BuildHasher>(shell: &str, env: &HashMap<String, String, S>) -> ShellPlan {
    let name = Path::new(shell).file_name().and_then(|name| name.to_str()).unwrap_or(shell);
    match name {
        "zsh" => zsh_plan(shell, env),
        "bash" => bash_plan(shell, env),
        "fish" => fish_plan(shell, env),
        _ => ShellPlan {
            program: shell.to_owned(),
            args: vec!["-l".to_owned()],
            env: clone_env(env),
            shim: None,
        },
    }
}

fn zsh_plan<S: std::hash::BuildHasher>(shell: &str, env: &HashMap<String, String, S>) -> ShellPlan {
    let real = env
        .get("ZDOTDIR")
        .cloned()
        .or_else(|| env.get("HOME").cloned())
        .unwrap_or_else(|| "/".to_owned());
    let Some(shim) = make_shim("zsh") else {
        return ShellPlan {
            program: shell.to_owned(),
            args: vec!["-l".to_owned()],
            env: clone_env(env),
            shim: None,
        };
    };
    // **zsh reads .zshenv from ZDOTDIR for every invocation**, before the
    // profile and rc, so the shim has to forward the person's own or their
    // environment is silently dropped.
    let zshenv = format!("if [ -f \"{real}/.zshenv\" ]; then source \"{real}/.zshenv\"; fi\n");
    let zshrc = format!("if [ -f \"{real}/.zshrc\" ]; then source \"{real}/.zshrc\"; fi\n{ZSH}");
    let zprofile =
        format!("if [ -f \"{real}/.zprofile\" ]; then source \"{real}/.zprofile\"; fi\n");
    let zshenv_path = shim.join(".zshenv");
    let zshrc_path = shim.join(".zshrc");
    let zprofile_path = shim.join(".zprofile");
    for (path, body) in
        [(&zshenv_path, &zshenv), (&zshrc_path, &zshrc), (&zprofile_path, &zprofile)]
    {
        if let Err(error) = std::fs::write(path, body) {
            warn_unavailable("zsh", path, &error);
            let _ = std::fs::remove_dir_all(&shim);
            return ShellPlan {
                program: shell.to_owned(),
                args: vec!["-l".to_owned()],
                env: clone_env(env),
                shim: None,
            };
        }
    }
    let mut env = clone_env(env);
    env.insert("ZDOTDIR".to_owned(), shim.display().to_string());
    env.insert("FORGE_REAL_ZDOTDIR".to_owned(), real);
    ShellPlan { program: shell.to_owned(), args: vec!["-l".to_owned()], env, shim: Some(shim) }
}

fn bash_plan<S: std::hash::BuildHasher>(
    shell: &str,
    env: &HashMap<String, String, S>,
) -> ShellPlan {
    let home = env.get("HOME").cloned().unwrap_or_else(|| "/".to_owned());
    let Some(shim) = make_shim("bash") else {
        return ShellPlan {
            program: shell.to_owned(),
            args: vec!["-i".to_owned()],
            env: clone_env(env),
            shim: None,
        };
    };
    let rc = format!("if [ -f \"{home}/.bashrc\" ]; then source \"{home}/.bashrc\"; fi\n{BASH}");
    let rcfile = shim.join("bashrc");
    if let Err(error) = std::fs::write(&rcfile, rc) {
        warn_unavailable("bash", &rcfile, &error);
        let _ = std::fs::remove_dir_all(&shim);
        return ShellPlan {
            program: shell.to_owned(),
            args: vec!["-i".to_owned()],
            env: clone_env(env),
            shim: None,
        };
    }
    // **Not `-l`.** bash 3.2 ignores `--rcfile` in a login shell, which would
    // silently cost the marks the terminal__wait exactness rests on; the
    // rcfile sources the person's `~/.bashrc`, so their aliases still load.
    ShellPlan {
        program: shell.to_owned(),
        args: vec!["--rcfile".to_owned(), rcfile.display().to_string(), "-i".to_owned()],
        env: clone_env(env),
        shim: Some(shim),
    }
}

fn fish_plan<S: std::hash::BuildHasher>(
    shell: &str,
    env: &HashMap<String, String, S>,
) -> ShellPlan {
    let Some(shim) = make_shim("fish") else {
        return ShellPlan {
            program: shell.to_owned(),
            args: vec!["-l".to_owned()],
            env: clone_env(env),
            shim: None,
        };
    };
    let script = shim.join("integration.fish");
    if let Err(error) = std::fs::write(&script, FISH) {
        warn_unavailable("fish", &script, &error);
        let _ = std::fs::remove_dir_all(&shim);
        return ShellPlan {
            program: shell.to_owned(),
            args: vec!["-l".to_owned()],
            env: clone_env(env),
            shim: None,
        };
    }
    ShellPlan {
        program: shell.to_owned(),
        args: vec!["-l".to_owned(), "-C".to_owned(), format!("source {}", script.display())],
        env: clone_env(env),
        shim: Some(shim),
    }
}

/// The environment the shell starts with: the seat's own, plus what the PTY's
/// terminal type answers to. The seat's env carries forge's launch terminal's
/// `TERM`, which is not what this shell is attached to.
fn clone_env<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> =
        env.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
    env.insert("TERM".to_owned(), "xterm-256color".to_owned());
    env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
    env
}

/// A fresh shim directory under the system temp dir.
fn make_shim(shell: &str) -> Option<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir =
        std::env::temp_dir().join(format!("forge-terminal-{shell}-{}-{nanos}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        warn_unavailable(shell, &dir, &error);
        return None;
    }
    Some(dir)
}

/// **A missing integration is said, not discovered later as a password
/// prompt.** Without the marks the shell falls back to the heuristic, which
/// is forge's own problem to know about.
fn warn_unavailable(shell: &str, path: &Path, error: &std::io::Error) {
    tracing::warn!(
        event_name = "terminal_integration_unavailable",
        shell,
        path = %path.display(),
        %error,
        "the shell integration could not be written; the terminal starts without OSC 133 marks"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("HOME".to_owned(), "/home/person".to_owned());
        env
    }

    /// zsh starts as a login shell with a `ZDOTDIR` shim, and the shim
    /// sources the person's own `.zshrc` before the integration.
    #[test]
    fn zsh_gets_a_zdotdir_shim_that_keeps_the_persons_rc() {
        let plan = plan("/bin/zsh", &env());
        assert_eq!(plan.program, "/bin/zsh");
        assert_eq!(plan.args, ["-l"]);
        let shim = plan.shim.as_ref().expect("a shim");
        assert_eq!(plan.env.get("ZDOTDIR"), Some(&shim.display().to_string()));
        assert_eq!(plan.env.get("FORGE_REAL_ZDOTDIR"), Some(&"/home/person".to_owned()));
        assert_eq!(plan.env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(plan.env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        let zshenv = std::fs::read_to_string(shim.join(".zshenv")).expect("a .zshenv");
        assert!(
            zshenv.contains("source \"/home/person/.zshenv\""),
            "the person's env file is sourced, as zsh always reads it"
        );
        let zshrc = std::fs::read_to_string(shim.join(".zshrc")).expect("a .zshrc");
        assert!(zshrc.contains("source \"/home/person/.zshrc\""), "the person's rc is sourced");
        assert!(zshrc.contains("133;D"), "and the integration follows it");
        plan.cleanup();
        assert!(!shim.exists(), "the shim is removed");
    }

    /// bash starts with `--rcfile`, and the rcfile sources `~/.bashrc` first.
    #[test]
    fn bash_gets_an_rcfile_that_keeps_the_persons_rc() {
        let plan = plan("/bin/bash", &env());
        assert_eq!(plan.program, "/bin/bash");
        assert_eq!(plan.args[0], "--rcfile");
        assert_eq!(plan.args[2], "-i");
        let rcfile = PathBuf::from(&plan.args[1]);
        let rc = std::fs::read_to_string(&rcfile).expect("an rcfile");
        assert!(rc.contains("source \"/home/person/.bashrc\""), "the person's rc is sourced");
        assert!(rc.contains("133;D"), "and the integration follows it");
        plan.cleanup();
    }

    /// fish gets its init command, which sources the integration.
    #[test]
    fn fish_gets_an_init_command() {
        let plan = plan("/opt/homebrew/bin/fish", &env());
        assert_eq!(plan.program, "/opt/homebrew/bin/fish");
        assert_eq!(plan.args[0], "-l", "a login shell, so the person's config ran");
        assert_eq!(plan.args[1], "-C");
        assert!(plan.args[2].starts_with("source "), "{}", plan.args[2]);
        plan.cleanup();
    }

    /// A shell forge does not know starts with no integration, so the
    /// classifier falls back rather than pretending.
    #[test]
    fn an_unknown_shell_gets_no_integration() {
        let plan = plan("/bin/sh", &env());
        assert!(plan.shim.is_none());
        assert_eq!(plan.args, ["-l"]);
    }
}
