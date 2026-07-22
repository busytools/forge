use ratatui::style::Color;

// Accent
pub const RUST_ORANGE: Color = Color::Rgb(244, 118, 0);

// UI chrome
pub const DIM: Color = Color::DarkGray;
pub const PROMPT_CHAR: &str = "\u{27a4}";

// Role header colors
pub const ROLE_ASSISTANT: Color = RUST_ORANGE;

// User message background
pub const USER_MSG_BG: Color = Color::Rgb(40, 44, 52);

// Tool status icons
pub const ICON_COMPLETED: &str = "\u{2713}";
pub const ICON_FAILED: &str = "\u{2717}";

// Status colors
pub const STATUS_ERROR: Color = Color::Red;
pub const STATUS_WARNING: Color = Color::Yellow;
pub const SLASH_COMMAND: Color = Color::LightMagenta;
pub const SUBAGENT_TOKEN: Color = Color::LightBlue;

// Resolved review-thread accent - a muted green distinct from the diff
// addition surface, for the collapsed "✓ RESOLVED" review-comment row.
pub const REVIEW_RESOLVED: Color = Color::Rgb(130, 199, 107);

// Amber accent for the `/account` picker's experimental tag. Distinct
// from the yellow reset-ETA (STATUS_WARNING) so the two never blur.
pub const EXPERIMENTAL: Color = Color::Rgb(201, 161, 59);

// Gotify external-notification accent - the ◈ gotify glyph + the `Gotify`
// source label in the chat notification block. Cyan, distinct from
// RUST_ORANGE (peer / agent traffic).
pub const GOTIFY: Color = Color::Rgb(78, 201, 201);

// Diff row background tints - GitHub dark-mode added / removed surface.
// Single source of truth shared by the /diff overlay and the
// Edit-tool inline diff renderer.
//
// Values match GitHub's `--bgColor-success-muted` / `--bgColor-danger-muted`
// pre-composited against the dark-default canvas (rgba(46, 160, 67, 0.15)
// and rgba(248, 81, 73, 0.15) over #0d1117 respectively, with a small
// terminal-visibility bump). The prior values were too saturated - the
// red especially read as a vivid danger flash rather than a quiet
// "deletion" surface; the green was less egregious but still bright.
pub const DIFF_ADDITION_BG: Color = Color::Rgb(15, 49, 30);
pub const DIFF_DELETION_BG: Color = Color::Rgb(58, 22, 26);

/// Filled-bar background for the `/diff` overlay's per-file sticky
/// header, so each file's start reads as a banded divider rather than
/// blending into the surrounding diff lines. A cool slate that sits
/// above the canvas without competing with the green / red line tints.
pub const DIFF_FILE_HEADER_BG: Color = Color::Rgb(27, 33, 48);

/// SDK tool icon + label pair. Monochrome Unicode symbols.
/// Unknown tool names fall back to a generic Tool label.
pub fn tool_name_label(sdk_tool_name: &str) -> (&'static str, &'static str) {
    match sdk_tool_name {
        "Read" => ("\u{2b1a}", "Read"),
        "Write" => ("\u{25a3}", "Write"),
        "Edit" => ("\u{25a3}", "Edit"),
        "MultiEdit" => ("\u{25a3}", "MultiEdit"),
        "NotebookEdit" => ("\u{25a3}", "NotebookEdit"),
        "Delete" => ("\u{25a3}", "Delete"),
        "Move" => ("\u{21c4}", "Move"),
        "Glob" => ("\u{2315}", "Glob"),
        "Grep" => ("\u{2315}", "Grep"),
        "LS" => ("\u{2315}", "LS"),
        "Bash" => ("\u{25b6}", "Bash"),
        "Task" | "Agent" => ("\u{25c7}", "Subagent"),
        "WebFetch" | "web_fetch" => ("\u{2295}", "WebFetch"),
        "WebSearch" | "web_search" => ("\u{2295}", "WebSearch"),
        "ExitPlanMode" | "EnterPlanMode" => (
            "\u{2299}",
            match sdk_tool_name {
                "EnterPlanMode" => "EnterPlanMode",
                _ => "ExitPlanMode",
            },
        ),
        // CLI 2.1.156 task surface (#268). The per-row chrome stays
        // chat-suppressed so the glyph only surfaces when a future
        // Inspector view exposes the raw tool calls (today: never).
        "TaskCreate" => ("\u{25cc}", "TaskCreate"),
        "TaskUpdate" => ("\u{25cc}", "TaskUpdate"),
        "TaskList" => ("\u{25cc}", "TaskList"),
        "TaskGet" => ("\u{25cc}", "TaskGet"),
        "Config" => ("\u{2299}", "Config"),
        "EnterWorktree" | "ExitWorktree" => (
            "\u{21c4}",
            match sdk_tool_name {
                "ExitWorktree" => "ExitWorktree",
                _ => "EnterWorktree",
            },
        ),
        // CLI 2.1.156 tool surface (#273). 13 new tool name glyphs +
        // Workflow's distinct filled-diamond marker.
        "ScheduleWakeup" => ("\u{23f2}", "ScheduleWakeup"),
        "Skill" => ("\u{2726}", "Skill"),
        "ToolSearch" | "tool_search_tool_regex" | "tool_search_tool_bm25" => {
            ("\u{2316}", "ToolSearch")
        }
        // Cron* family shares the ASCII `*` glyph (cron-syntax mapping
        // `* * * * *`); width-1 keeps the kind-icon slot deterministic
        // across terminals regardless of EAW interpretation. Per-arm
        // label preserves the originating tool name for log diagnostics.
        "CronCreate" => ("*", "CronCreate"),
        "CronDelete" => ("*", "CronDelete"),
        "CronList" => ("*", "CronList"),
        "PushNotification" => ("\u{25b2}", "PushNotification"),
        "RemoteTrigger" => ("\u{21e8}", "RemoteTrigger"),
        "LSP" => ("\u{2699}", "LSP"),
        "TaskOutput" => ("\u{25c9}", "TaskOutput"),
        "TaskStop" => ("\u{25cd}", "TaskStop"),
        // Filled diamond ◆ - agent-script flow, distinct from
        // Task/Agent's hollow ◇ subagent-dispatch glyph.
        "Workflow" => ("\u{25c6}", "Workflow"),
        // `advisor` is the upstream server-tool wire name for the
        // model-side advisor call. Borrows the Skill sparkle (✦) since
        // both surface model-side counsel without a local handler. The
        // other server-tool wire names (`tool_search_tool_regex`,
        // `tool_search_tool_bm25`, `web_search`, `web_fetch`) share the
        // in-process arms above so the card chrome stays visually
        // consistent regardless of which side of the wire the call
        // came from.
        "advisor" => ("\u{2726}", "Advisor"),
        // CLI 2.1.204 tool surface (new names in the init tool list).
        "DesignSync" => ("\u{21bb}", "DesignSync"),
        "ReportFindings" => ("\u{25a4}", "ReportFindings"),
        "SendMessage" => ("\u{27a4}", "SendMessage"),
        "ShareOnboardingGuide" => ("\u{29c9}", "ShareOnboardingGuide"),
        _ => ("\u{25cb}", "Tool"),
    }
}

#[cfg(test)]
mod tests {
    use super::tool_name_label;

    #[test]
    fn task_and_agent_share_subagent_label_and_icon() {
        assert_eq!(tool_name_label("Task"), ("\u{25c7}", "Subagent"));
        assert_eq!(tool_name_label("Agent"), ("\u{25c7}", "Subagent"));
    }

    /// #273: CLI 2.1.156 tool surface. Glyph picks locked in the
    /// plan file (`~/Projects/forge/.claude/plans/273.md` glyph
    /// table). Each entry asserts both glyph + label so a future
    /// edit that swaps either surfaces here.
    #[test]
    fn cli_2_1_156_tool_glyphs_match_plan_picks() {
        // Reused-glyph row: EnterPlanMode shares the ⊙ Config glyph;
        // ExitWorktree shares the ⇄ EnterWorktree glyph.
        assert_eq!(tool_name_label("EnterPlanMode"), ("\u{2299}", "EnterPlanMode"));
        assert_eq!(tool_name_label("ExitWorktree"), ("\u{21c4}", "ExitWorktree"));

        // New-glyph rows.
        assert_eq!(tool_name_label("ScheduleWakeup"), ("\u{23f2}", "ScheduleWakeup"));
        assert_eq!(tool_name_label("Skill"), ("\u{2726}", "Skill"));
        assert_eq!(tool_name_label("ToolSearch"), ("\u{2316}", "ToolSearch"));

        // Cron family shares the ASCII `*` glyph.
        assert_eq!(tool_name_label("CronCreate"), ("*", "CronCreate"));
        assert_eq!(tool_name_label("CronDelete"), ("*", "CronDelete"));
        assert_eq!(tool_name_label("CronList"), ("*", "CronList"));

        assert_eq!(tool_name_label("PushNotification"), ("\u{25b2}", "PushNotification"));
        assert_eq!(tool_name_label("RemoteTrigger"), ("\u{21e8}", "RemoteTrigger"));
        assert_eq!(tool_name_label("LSP"), ("\u{2699}", "LSP"));
        assert_eq!(tool_name_label("TaskOutput"), ("\u{25c9}", "TaskOutput"));
        assert_eq!(tool_name_label("TaskStop"), ("\u{25cd}", "TaskStop"));

        // Workflow is the filled diamond ◆, distinct from Task/Agent's
        // hollow ◇ - agent-script flow vs subagent dispatch.
        assert_eq!(tool_name_label("Workflow"), ("\u{25c6}", "Workflow"));
    }

    /// Server-side tool wire names land here too (e.g. ToolSearch
    /// arrives as `tool_search_tool_regex` or `_bm25`, not the
    /// in-process `ToolSearch`). Without a display arm they fall to
    /// the generic `("\u{25cb}", "Tool")` and the card chrome is
    /// meaningless. Reuse the in-process glyphs so the visual stays
    /// stable regardless of which side of the wire the call comes
    /// from.
    #[test]
    fn server_tool_wire_names_map_to_friendly_labels() {
        assert_eq!(tool_name_label("tool_search_tool_regex"), ("\u{2316}", "ToolSearch"));
        assert_eq!(tool_name_label("tool_search_tool_bm25"), ("\u{2316}", "ToolSearch"));
        assert_eq!(tool_name_label("web_search"), ("\u{2295}", "WebSearch"));
        assert_eq!(tool_name_label("web_fetch"), ("\u{2295}", "WebFetch"));
        assert_eq!(tool_name_label("advisor"), ("\u{2726}", "Advisor"));
    }

    /// CLI 2.1.204 tool surface. Asserts glyph + label so a future
    /// edit that swaps either surfaces here.
    #[test]
    fn cli_2_1_204_tool_glyphs_match_picks() {
        assert_eq!(tool_name_label("DesignSync"), ("\u{21bb}", "DesignSync"));
        assert_eq!(tool_name_label("ReportFindings"), ("\u{25a4}", "ReportFindings"));
        assert_eq!(tool_name_label("SendMessage"), ("\u{27a4}", "SendMessage"));
        assert_eq!(tool_name_label("ShareOnboardingGuide"), ("\u{29c9}", "ShareOnboardingGuide"));
    }
}
