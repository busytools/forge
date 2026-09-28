//! forge's own slash commands: the catalogue a view offers beside the
//! ones the CLI advertises. View-side data with no session in it, so it
//! lives here rather than in either view: the table is the same for every
//! reader, and a second copy would drift.

/// One command forge handles itself: the name as it is typed, and what it
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeCommand {
    pub name: &'static str,
    pub description: &'static str,
}

/// Every command a session supports that is forge's own, alphabetical by
/// name, which is the order a dropdown draws them in.
pub const FORGE_COMMANDS: &[ForgeCommand] = &[
    ForgeCommand { name: "/compact", description: "Compact session context" },
    ForgeCommand {
        name: "/dictate",
        description: "Set how dictation is cleaned up, for this session",
    },
    ForgeCommand { name: "/diff", description: "Review changes in a full-screen diff overlay" },
    ForgeCommand { name: "/effort", description: "Show / set thinking effort" },
    ForgeCommand { name: "/extensions", description: "Open extensions" },
    ForgeCommand { name: "/gateway", description: "Inspect the gateway's orgs and accounts" },
    ForgeCommand { name: "/launchpad", description: "Return to project picker" },
    ForgeCommand { name: "/mode", description: "Show / set session mode" },
    ForgeCommand { name: "/model", description: "Show / set session model" },
    ForgeCommand { name: "/new", description: "Start a fresh session" },
    ForgeCommand { name: "/resume", description: "Resume a session by ID" },
    ForgeCommand { name: "/spinner", description: "Show / set the spinner style" },
    ForgeCommand { name: "/usage", description: "Token/cost usage by project or model" },
];

/// Whether forge handles `name` itself. A view rendering the CLI's own
/// list skips these, so one command is not drawn twice.
pub fn is_forge_command(name: &str) -> bool {
    FORGE_COMMANDS.iter().any(|command| command.name == name)
}
