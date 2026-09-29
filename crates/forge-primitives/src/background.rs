//! The CLI's background-task registry, as a view reads it.
//!
//! The CLI announces the whole set on `background_tasks_changed` and links
//! each task to the tool call that began it on `task_started`. Both are held
//! on the session, because a view that attached after they arrived has no
//! other way to learn what is running: the update stream carries them once.

use serde::{Deserialize, Serialize};

/// One entry of the CLI's background-task registry, with the command its own
/// card carried where forge has seen both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub task_id: String,
    /// The kind a view routes the row on: `local_bash` is the processes feed,
    /// an agent kind the sub-agent section.
    pub task_type: String,
    /// The line the row leads with, as the CLI wrote it.
    pub description: String,
    /// The command the task's own tool call carried, which is what an OS scan
    /// adopts a detached process by. `None` when forge has not seen the card:
    /// the task still draws, without one.
    pub command: Option<String>,
}
