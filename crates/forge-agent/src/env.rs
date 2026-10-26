//! Live environment - git context, cwd insights, OS-side
//! observations the agent needs but the SDK doesn't.
//!
//! Distinct from `userdata` (on-disk Claude state) and `cloud`
//! (network-side talk to api.anthropic.com): `env` is local-machine
//! state about the project the user is working on right now.

pub mod cli_version;
pub mod config_watch;
pub mod documents;
pub mod file_index;
pub mod git_command;
pub mod git_diff;
pub mod git_state;
pub mod lsp_path;
pub mod processes;
pub mod timezone;
pub mod token_usage;
pub mod worktree;

/// Whether an event kind can change a watched path's content. A read
/// cannot, and it matters: on inotify a read of a watched path is itself
/// an event, so a watcher that reacts to one feeds itself.
pub(crate) fn is_content_change(kind: notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::ModifyKind;

    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Name(_))
    )
}
