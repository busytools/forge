//! The Extensions page's tab model and per-tab view state: which tab
//! is open, each tab's filter and selection, and the flattened
//! extension rows the tabs render.

use crate::app::input::InputState;
use forge_primitives::plugins::ExtensionRow;

/// One flat sibling tab per component kind. A second navigation level
/// would be heavy in a terminal, and each component row carries its
/// source plugin, which preserves the tier relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtensionsTab {
    #[default]
    Installed,
    Skills,
    Agents,
    Commands,
    Hooks,
    Lsp,
    Mcps,
    Marketplaces,
}

impl ExtensionsTab {
    pub const ALL: [Self; 8] = [
        Self::Installed,
        Self::Skills,
        Self::Agents,
        Self::Commands,
        Self::Hooks,
        Self::Lsp,
        Self::Mcps,
        Self::Marketplaces,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Installed => "Installed",
            Self::Skills => "Skills",
            Self::Agents => "Agents",
            Self::Commands => "Commands",
            Self::Hooks => "Hooks",
            Self::Lsp => "LSP",
            Self::Mcps => "MCPs",
            Self::Marketplaces => "Marketplaces",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Installed => Self::Skills,
            Self::Skills => Self::Agents,
            Self::Agents => Self::Commands,
            Self::Commands => Self::Hooks,
            Self::Hooks => Self::Lsp,
            Self::Lsp => Self::Mcps,
            Self::Mcps => Self::Marketplaces,
            Self::Marketplaces => Self::Installed,
        }
    }

    pub const fn prev(self) -> Self {
        match self {
            Self::Installed => Self::Marketplaces,
            Self::Skills => Self::Installed,
            Self::Agents => Self::Skills,
            Self::Commands => Self::Agents,
            Self::Hooks => Self::Commands,
            Self::Lsp => Self::Hooks,
            Self::Mcps => Self::Lsp,
            Self::Marketplaces => Self::Mcps,
        }
    }

    /// Position in [`Self::ALL`]; indexes the state's per-tab arrays.
    pub const fn index(self) -> usize {
        match self {
            Self::Installed => 0,
            Self::Skills => 1,
            Self::Agents => 2,
            Self::Commands => 3,
            Self::Hooks => 4,
            Self::Lsp => 5,
            Self::Mcps => 6,
            Self::Marketplaces => 7,
        }
    }

    /// Every tab except MCPs and Marketplaces filters over the
    /// flattened extension rows.
    pub const fn filters_rows(self) -> bool {
        !matches!(self, Self::Mcps | Self::Marketplaces)
    }
}

/// The flattened extension rows for one tab: the rows the tab renders
/// before filtering. Plugin rows appear only on the Installed tab;
/// every component tab filters by kind.
pub fn rows_for_tab(rows: &[ExtensionRow], tab: ExtensionsTab) -> Vec<&ExtensionRow> {
    use forge_primitives::plugins::ExtensionKind;
    match tab {
        ExtensionsTab::Installed => rows.iter().collect(),
        ExtensionsTab::Skills => {
            rows.iter().filter(|row| row.kind == ExtensionKind::Skill).collect()
        }
        ExtensionsTab::Agents => {
            rows.iter().filter(|row| row.kind == ExtensionKind::Agent).collect()
        }
        ExtensionsTab::Commands => {
            rows.iter().filter(|row| row.kind == ExtensionKind::Command).collect()
        }
        ExtensionsTab::Hooks => rows.iter().filter(|row| row.kind == ExtensionKind::Hook).collect(),
        ExtensionsTab::Lsp => rows.iter().filter(|row| row.kind == ExtensionKind::Lsp).collect(),
        ExtensionsTab::Mcps | ExtensionsTab::Marketplaces => Vec::new(),
    }
}

/// A row matches the filter when its name or source carries the query
/// (source is the plugin for components, the marketplace for plugin
/// rows), case-insensitively.
pub fn row_matches(row: &ExtensionRow, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    row.name.to_ascii_lowercase().contains(&query)
        || row.source.to_ascii_lowercase().contains(&query)
}

/// A row's live count for its tab, mirroring [`rows_for_tab`].
pub fn count_for_tab(rows: &[ExtensionRow], tab: ExtensionsTab) -> usize {
    if tab == ExtensionsTab::Installed {
        rows.iter()
            .filter(|row| row.kind == forge_primitives::plugins::ExtensionKind::Plugin)
            .count()
    } else {
        rows_for_tab(rows, tab).len()
    }
}

/// Per-tab input and selection, indexed by [`ExtensionsTab::index`].
#[derive(Clone, Debug)]
pub struct TabState {
    pub search_queries: Vec<InputState>,
    pub selected: Vec<usize>,
}

impl Default for TabState {
    fn default() -> Self {
        Self::new(ExtensionsTab::ALL.len())
    }
}

impl TabState {
    pub fn new(tabs: usize) -> Self {
        Self {
            search_queries: (0..tabs).map(|_| InputState::new()).collect(),
            selected: vec![0; tabs],
        }
    }
}

/// The Installed tab's update-all count: the rows offering Update
/// right now, which is exactly what `Update all (N)` queues.
pub fn update_all_count(rows: &[ExtensionRow]) -> usize {
    use forge_primitives::plugins::RowState;
    rows.iter().filter(|row| row.state == RowState::UpdateAvailable).count()
}
