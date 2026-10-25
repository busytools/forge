//! The seat's live tasks, as a view reads it.
//!
//! The CLI announces its whole background registry on
//! `background_tasks_changed`, and a long foreground call on `task_started`
//! and `task_notification` only - it never enters that registry. Both are
//! held on the session, because a view that attached after they arrived has
//! no other way to learn what is running: the update stream carries them once.

use serde::{Deserialize, Serialize};

/// One of the seat's live tasks, with the command its own card carried where
/// forge has seen both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub task_id: String,
    /// Whether the CLI put the call in the background, as the frame that
    /// announced it said. A registry entry is backgrounded by definition; a
    /// long foreground call the CLI tracks rides here with `false` while it
    /// runs, which is how a consumer tells the two kinds of hold apart.
    pub is_backgrounded: bool,
    /// The kind a view routes the row on: `local_bash` is the processes feed
    /// (a backgrounded call, or a foreground one the CLI tracks while it
    /// runs), an agent kind the sub-agent section.
    pub task_type: String,
    /// The line the row leads with, as the CLI wrote it.
    pub description: String,
    /// The command the task's own tool call carried, which is what an OS scan
    /// adopts a detached process by. `None` when forge has not seen the card:
    /// the task still draws, without one.
    pub command: Option<String>,
    /// The tool call that began the task, from the CLI's `task_started` link.
    /// `None` until forge has seen it: the row draws without a call to reach.
    pub tool_use_id: Option<String>,
}
