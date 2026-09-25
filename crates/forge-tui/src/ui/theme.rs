use forge_sessions::family::{ToolFamily, tool_family, tool_label};
use forge_sessions::grouping::KindRow;
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

// Fenced code block surface: a quiet panel lifted above the terminal
// canvas, with the fence's info string dimmed inside it.
pub const CODE_PANEL_BG: Color = Color::Rgb(23, 27, 35);
pub const CODE_PANEL_LABEL: Color = Color::Rgb(85, 92, 104);

// Tool status icons
pub const ICON_COMPLETED: &str = "\u{2713}";
pub const ICON_FAILED: &str = "\u{2717}";

// Status colors
pub const STATUS_ERROR: Color = Color::Red;
pub const STATUS_WARNING: Color = Color::Yellow;
pub const SLASH_COMMAND: Color = Color::LightMagenta;
pub const SUBAGENT_TOKEN: Color = Color::LightBlue;

// Available-not-installed rows on the Extensions page: the available
// stream's blue, kept apart from the installed tiers' green / orange /
// red.
pub const AVAILABLE: Color = Color::Rgb(111, 143, 163);

// Resolved review-thread accent - a muted green distinct from the diff
// addition surface, for the collapsed "✓ RESOLVED" review-comment row.
pub const REVIEW_RESOLVED: Color = Color::Rgb(130, 199, 107);

// Completion glyph on a Projects-pane row whose turn finished unseen.
pub const COMPLETION: Color = REVIEW_RESOLVED;

// Addressed review-thread accent - a blue for a thread a worker replied
// to (Open -> Addressed), and the worker turn's dot on the conversation
// rail. Distinct from REVIEW_RESOLVED (green) and RUST_ORANGE (the
// user's own turn).
pub const REVIEW_ADDRESSED: Color = Color::Rgb(97, 160, 224);

// Amber accent for the `/usage` view's GPT rows. Distinct from the
// yellow reset-ETA (STATUS_WARNING) so the two never blur.
pub const EXPERIMENTAL: Color = Color::Rgb(201, 161, 59);

// Gotify external-notification accent - the ◈ gotify glyph + the `Gotify`
// source label in the chat notification block. Cyan, distinct from
// RUST_ORANGE (peer / agent traffic).
pub const GOTIFY: Color = Color::Rgb(78, 201, 201);

// Slack external-notification accent - the ◇ glyph + the `Slack` source label
// in the chat notification block. Slack green, distinct from GOTIFY's cyan.
pub const SLACK: Color = Color::Rgb(46, 182, 125);

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
    (tool_glyph(sdk_tool_name), tool_label(sdk_tool_name))
}

/// The kind glyph for a tool. A family draws as one glyph; a tool with no
/// family keeps the glyph its own label carries.
pub fn tool_glyph(sdk_tool_name: &str) -> &'static str {
    family_glyph(tool_family(sdk_tool_name))
}

/// The glyph a family draws. `Own` keys on the family's own label - the same
/// label the tool's card carries - so a card and a summary row draw one
/// symbol for one tool.
fn family_glyph(family: ToolFamily) -> &'static str {
    match family {
        ToolFamily::Read => READ_GLYPH,
        ToolFamily::Search => "\u{2315}",
        ToolFamily::Bash => "\u{25b6}",
        ToolFamily::Web => "\u{2295}",
        ToolFamily::Lsp => "\u{2699}",
        ToolFamily::Skill => "\u{2726}",
        ToolFamily::ToolSearch => "\u{2316}",
        ToolFamily::Config => "\u{2299}",
        ToolFamily::Worktree => "\u{21c4}",
        ToolFamily::Tool => "\u{25cb}",
        ToolFamily::Own(label) => match label {
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Delete" => "\u{25a3}",
            "Subagent" => "\u{25c7}",
            // CLI 2.1.156 tool surface (#273).
            "ScheduleWakeup" => "\u{23f2}",
            "PushNotification" => "\u{25b2}",
            "TaskOutput" => "\u{25c9}",
            "TaskStop" => "\u{25cd}",
            // CLI 2.1.204 tool surface (new names in the init tool list).
            "DesignSync" => "\u{21bb}",
            "ReportFindings" => "\u{25a4}",
            "ShareOnboardingGuide" => "\u{29c9}",
            _ => "\u{25cb}",
        },
    }
}

/// The read glyph (`⬚`). The render special-cases read on it: it relativizes
/// each path against the project root and clips with a middle-ellipsis
/// (keeping the filename), where every other kind clips end-first.
pub const READ_GLYPH: &str = "\u{2b1a}";

/// L2 marker glyph for `mcp__<server>__*` lines - distinct from the generic
/// `\u{25cb}` so a server call reads apart from local tools.
pub const MCP_GLYPH: &str = "\u{25c8}";

/// The arrow a peer block draws, outbound and inbound. Also the glyph a
/// grouping summary draws for a peer envelope's row.
pub const OUTBOUND_GLYPH: &str = "\u{2934}";
pub const INBOUND_GLYPH: &str = "\u{2935}";

/// The glyph a grouping row draws. Every family row draws its family glyph;
/// a peer envelope draws its direction arrow, and an MCP row draws
/// [`MCP_GLYPH`].
pub fn kind_row_glyph(row: KindRow) -> &'static str {
    match row {
        KindRow::Family(family) => family_glyph(family),
        KindRow::Mcp => MCP_GLYPH,
        KindRow::Inbound => INBOUND_GLYPH,
        KindRow::Outbound => OUTBOUND_GLYPH,
    }
}

#[cfg(test)]
mod tests {
    use super::{KindRow, ToolFamily, kind_row_glyph, tool_family, tool_name_label};

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

        assert_eq!(tool_name_label("PushNotification"), ("\u{25b2}", "PushNotification"));
        assert_eq!(tool_name_label("LSP"), ("\u{2699}", "LSP"));
        assert_eq!(tool_name_label("TaskOutput"), ("\u{25c9}", "TaskOutput"));
        assert_eq!(tool_name_label("TaskStop"), ("\u{25cd}", "TaskStop"));
    }

    /// Every row a grouping summary can draw, against its literal glyph.
    /// The MCP row is the one that must not collapse into the generic tool
    /// row: a server call reads apart from a local tool.
    #[test]
    fn kind_rows_draw_their_family_glyph() {
        for (row, glyph) in [
            (KindRow::Family(ToolFamily::Read), "\u{2b1a}"),
            (KindRow::Family(ToolFamily::Search), "\u{2315}"),
            (KindRow::Family(ToolFamily::Bash), "\u{25b6}"),
            (KindRow::Family(ToolFamily::Web), "\u{2295}"),
            (KindRow::Family(ToolFamily::Lsp), "\u{2699}"),
            (KindRow::Family(ToolFamily::Skill), "\u{2726}"),
            (KindRow::Family(ToolFamily::ToolSearch), "\u{2316}"),
            (KindRow::Family(ToolFamily::Config), "\u{2299}"),
            (KindRow::Family(ToolFamily::Worktree), "\u{21c4}"),
            (KindRow::Family(ToolFamily::Tool), "\u{25cb}"),
            (KindRow::Mcp, "\u{25c8}"),
            (KindRow::Inbound, "\u{2935}"),
            (KindRow::Outbound, "\u{2934}"),
        ] {
            assert_eq!(kind_row_glyph(row), glyph, "{row:?} draws its own glyph");
        }
    }

    /// A family that is a tool's own label draws the glyph its row has always
    /// drawn. `Own` keys on the label rather than the name, so this covers
    /// every `Own` label `family.rs` has a row for.
    ///
    /// It does NOT enumerate that table: a tool added to `family.rs` with no
    /// glyph here falls to the generic row, and only the glyph table's own
    /// `_` arm is what catches it.
    #[test]
    fn own_family_rows_keep_their_own_glyphs() {
        for (name, glyph) in [
            ("Write", "\u{25a3}"),
            ("Edit", "\u{25a3}"),
            ("MultiEdit", "\u{25a3}"),
            ("NotebookEdit", "\u{25a3}"),
            ("Delete", "\u{25a3}"),
            ("Task", "\u{25c7}"),
            ("Agent", "\u{25c7}"),
            ("ScheduleWakeup", "\u{23f2}"),
            ("PushNotification", "\u{25b2}"),
            ("TaskOutput", "\u{25c9}"),
            ("TaskStop", "\u{25cd}"),
            ("DesignSync", "\u{21bb}"),
            ("ReportFindings", "\u{25a4}"),
            ("ShareOnboardingGuide", "\u{29c9}"),
        ] {
            assert_eq!(
                kind_row_glyph(KindRow::Family(tool_family(name))),
                glyph,
                "{name} draws its own glyph"
            );
        }
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
        assert_eq!(tool_name_label("ShareOnboardingGuide"), ("\u{29c9}", "ShareOnboardingGuide"));
    }
}
