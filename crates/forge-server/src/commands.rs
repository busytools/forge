//! forge's own slash commands: the catalogue a view offers beside the
//! ones the CLI advertises. View-side data with no session in it, so it
//! lives here rather than in either view: the table is the same for every
//! reader, and a second copy would drift.
//!
//! **Every name here is one the surviving stack answers.** The terminal's
//! own handlers (`/diff`, `/spinner`, `/dictate`, `/gateway`,
//! `/launchpad`) went with it rather than being ported: a name advertised
//! here and answered nowhere is worse than an absent one, because typing
//! it reaches the model as prose. Their filed issues are the later menu.
//!
//! **`/usage` describes the CLI's own answer.** The name is a CLI builtin,
//! and the client's page that answered it is gone, so the line must state
//! what typing it draws rather than that page's report.

/// One command in the catalogue: the name as it is typed, and what it
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeCommand {
    pub name: &'static str,
    pub description: &'static str,
}

/// Every command in the catalogue, alphabetical by name, which is the
/// order a dropdown draws them in.
pub const FORGE_COMMANDS: &[ForgeCommand] = &[
    ForgeCommand { name: "/compact", description: "Compact session context" },
    ForgeCommand { name: "/effort", description: "Set thinking effort" },
    ForgeCommand { name: "/mode", description: "Set session mode" },
    ForgeCommand { name: "/model", description: "Show / set session model" },
    ForgeCommand { name: "/new", description: "Start a fresh session" },
    ForgeCommand { name: "/resume", description: "Resume a session by ID" },
    ForgeCommand { name: "/usage", description: "Show session cost and plan usage" },
];

/// Whether `name` is in the catalogue. A view rendering the CLI's own
/// list skips these, so one command is not drawn twice.
pub fn is_forge_command(name: &str) -> bool {
    FORGE_COMMANDS.iter().any(|command| command.name == name)
}
