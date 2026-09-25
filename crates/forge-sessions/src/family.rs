//! Which family a tool belongs to, and the label its own row carries.
//!
//! A family is a concept; a glyph is one rendering of it, so the glyphs stay in
//! the theme. Grouping keys its kind rows on this rather than on the glyph
//! string the theme returns, which is what lets the grouping policy leave the
//! view crate without dragging a glyph table with it.

/// The group a tool's calls are summarised under: the merge key for a kind
/// row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolFamily {
    Read,
    Search,
    Bash,
    Web,
    Lsp,
    Skill,
    ToolSearch,
    Config,
    Worktree,
    /// Every tool this crate has no row for shares one generic row.
    Tool,
    /// A tool with a glyph and label of its own, and no family to share.
    Own(&'static str),
}

impl ToolFamily {
    /// The label the kind row shows: the family's own name, `tool` for the
    /// generic row, or the tool's own label.
    pub const fn kind_label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Search => "search",
            Self::Bash => "bash",
            Self::Web => "web",
            Self::Lsp => "lsp",
            Self::Skill => "skill",
            Self::ToolSearch => "toolsearch",
            Self::Config => "config",
            Self::Worktree => "worktree",
            Self::Tool => "tool",
            Self::Own(label) => label,
        }
    }
}

/// The family a tool name belongs to. A name with a label row of its own keeps
/// its own row; one with nothing falls into the generic `tool` row.
pub fn tool_family(sdk_tool_name: &str) -> ToolFamily {
    match sdk_tool_name {
        "Read" => ToolFamily::Read,
        "Glob" | "Grep" | "LS" => ToolFamily::Search,
        "Bash" => ToolFamily::Bash,
        "WebFetch" | "web_fetch" | "WebSearch" | "web_search" => ToolFamily::Web,
        "LSP" => ToolFamily::Lsp,
        "Skill" | "advisor" => ToolFamily::Skill,
        "ToolSearch" | "tool_search_tool_regex" | "tool_search_tool_bm25" => ToolFamily::ToolSearch,
        "ExitPlanMode" | "EnterPlanMode" | "Config" => ToolFamily::Config,
        "Move" | "EnterWorktree" | "ExitWorktree" => ToolFamily::Worktree,
        _ => match row_label(sdk_tool_name) {
            Some(label) => ToolFamily::Own(label),
            None => ToolFamily::Tool,
        },
    }
}

/// The label a tool's own row and card carry, and the prefix a chat title is
/// stripped of. Unknown names fall back to `Tool`.
pub fn tool_label(sdk_tool_name: &str) -> &'static str {
    row_label(sdk_tool_name).unwrap_or("Tool")
}

/// The label row for a tool, or `None` when the table has no row for the name -
/// which is what makes the name a [`ToolFamily::Tool`] rather than a row of its
/// own.
fn row_label(sdk_tool_name: &str) -> Option<&'static str> {
    Some(match sdk_tool_name {
        "Read" => "Read",
        "Write" => "Write",
        "Edit" => "Edit",
        "MultiEdit" => "MultiEdit",
        "NotebookEdit" => "NotebookEdit",
        "Delete" => "Delete",
        "Move" => "Move",
        "Glob" => "Glob",
        "Grep" => "Grep",
        "LS" => "LS",
        "Bash" => "Bash",
        "Task" | "Agent" => "Subagent",
        "WebFetch" | "web_fetch" => "WebFetch",
        "WebSearch" | "web_search" => "WebSearch",
        "ExitPlanMode" | "EnterPlanMode" => match sdk_tool_name {
            "EnterPlanMode" => "EnterPlanMode",
            _ => "ExitPlanMode",
        },
        "Config" => "Config",
        "EnterWorktree" | "ExitWorktree" => match sdk_tool_name {
            "ExitWorktree" => "ExitWorktree",
            _ => "EnterWorktree",
        },
        "ScheduleWakeup" => "ScheduleWakeup",
        "Skill" => "Skill",
        "advisor" => "Advisor",
        "ToolSearch" | "tool_search_tool_regex" | "tool_search_tool_bm25" => "ToolSearch",
        "PushNotification" => "PushNotification",
        "LSP" => "LSP",
        "TaskOutput" => "TaskOutput",
        "TaskStop" => "TaskStop",
        "DesignSync" => "DesignSync",
        "ReportFindings" => "ReportFindings",
        "ShareOnboardingGuide" => "ShareOnboardingGuide",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{ToolFamily, tool_family, tool_label};

    /// One row per tool name the view knows: its family, and the label its own
    /// row carries. `Monitor` and `brand_new_tool` stand in for a name with no
    /// row at all.
    const TOOLS: &[(&str, ToolFamily, &str)] = &[
        ("Read", ToolFamily::Read, "Read"),
        ("Glob", ToolFamily::Search, "Glob"),
        ("Grep", ToolFamily::Search, "Grep"),
        ("LS", ToolFamily::Search, "LS"),
        ("Bash", ToolFamily::Bash, "Bash"),
        ("WebFetch", ToolFamily::Web, "WebFetch"),
        ("web_fetch", ToolFamily::Web, "WebFetch"),
        ("WebSearch", ToolFamily::Web, "WebSearch"),
        ("web_search", ToolFamily::Web, "WebSearch"),
        ("LSP", ToolFamily::Lsp, "LSP"),
        ("Skill", ToolFamily::Skill, "Skill"),
        ("advisor", ToolFamily::Skill, "Advisor"),
        ("ToolSearch", ToolFamily::ToolSearch, "ToolSearch"),
        ("tool_search_tool_regex", ToolFamily::ToolSearch, "ToolSearch"),
        ("tool_search_tool_bm25", ToolFamily::ToolSearch, "ToolSearch"),
        ("ExitPlanMode", ToolFamily::Config, "ExitPlanMode"),
        ("EnterPlanMode", ToolFamily::Config, "EnterPlanMode"),
        ("Config", ToolFamily::Config, "Config"),
        ("Move", ToolFamily::Worktree, "Move"),
        ("EnterWorktree", ToolFamily::Worktree, "EnterWorktree"),
        ("ExitWorktree", ToolFamily::Worktree, "ExitWorktree"),
        ("Write", ToolFamily::Own("Write"), "Write"),
        ("Edit", ToolFamily::Own("Edit"), "Edit"),
        ("MultiEdit", ToolFamily::Own("MultiEdit"), "MultiEdit"),
        ("NotebookEdit", ToolFamily::Own("NotebookEdit"), "NotebookEdit"),
        ("Delete", ToolFamily::Own("Delete"), "Delete"),
        ("Task", ToolFamily::Own("Subagent"), "Subagent"),
        ("Agent", ToolFamily::Own("Subagent"), "Subagent"),
        ("ScheduleWakeup", ToolFamily::Own("ScheduleWakeup"), "ScheduleWakeup"),
        ("PushNotification", ToolFamily::Own("PushNotification"), "PushNotification"),
        ("TaskOutput", ToolFamily::Own("TaskOutput"), "TaskOutput"),
        ("TaskStop", ToolFamily::Own("TaskStop"), "TaskStop"),
        ("DesignSync", ToolFamily::Own("DesignSync"), "DesignSync"),
        ("ReportFindings", ToolFamily::Own("ReportFindings"), "ReportFindings"),
        ("ShareOnboardingGuide", ToolFamily::Own("ShareOnboardingGuide"), "ShareOnboardingGuide"),
        ("Monitor", ToolFamily::Tool, "Tool"),
        ("brand_new_tool", ToolFamily::Tool, "Tool"),
        // Dropped from the theme's table, so they share the generic row; a
        // transcript that already carries one still replays it.
        ("CronCreate", ToolFamily::Tool, "Tool"),
        ("CronDelete", ToolFamily::Tool, "Tool"),
        ("CronList", ToolFamily::Tool, "Tool"),
        ("RemoteTrigger", ToolFamily::Tool, "Tool"),
        ("Workflow", ToolFamily::Tool, "Tool"),
        ("SendMessage", ToolFamily::Tool, "Tool"),
    ];

    #[test]
    fn tool_family_puts_every_tool_in_its_family() {
        for (name, family, _) in TOOLS {
            assert_eq!(tool_family(name), *family, "family for {name}");
        }
    }

    #[test]
    fn tool_label_names_every_tool() {
        for (name, _, label) in TOOLS {
            assert_eq!(tool_label(name), *label, "label for {name}");
        }
    }

    /// A family names its own kind row; the tool's own label stands in only
    /// where there is no family.
    #[test]
    fn kind_label_is_the_family_name_or_the_tools_own() {
        for (family, expected) in [
            (ToolFamily::Read, "read"),
            (ToolFamily::Search, "search"),
            (ToolFamily::Bash, "bash"),
            (ToolFamily::Web, "web"),
            (ToolFamily::Lsp, "lsp"),
            (ToolFamily::Skill, "skill"),
            (ToolFamily::ToolSearch, "toolsearch"),
            (ToolFamily::Config, "config"),
            (ToolFamily::Worktree, "worktree"),
            (ToolFamily::Tool, "tool"),
            (ToolFamily::Own("Write"), "Write"),
        ] {
            assert_eq!(family.kind_label(), expected, "kind row label for {family:?}");
        }
    }
}
