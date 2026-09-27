//! `slash_commands()`, `subagents()`, `file_index()`, `emoji()`: what
//! the composer's four autocomplete triggers read.
//!
//! Two of them are facts about a session - what the CLI advertised - so
//! the core holds them and these verbs read through it. The other two
//! have no session in them at all: the emoji table and the file walk are
//! view-side data, so `forge-sessions` owns both and no view keeps a
//! copy.

use std::path::Path;

use forge_primitives::{AvailableAgent, AvailableCommand, SessionSlot};

use crate::emoji::{self, Emoji};
use crate::file_index::FileIndex;
use crate::surface::ViewSurface;

impl ViewSurface {
    /// The slash commands the CLI last advertised for `slot`. Empty for a
    /// seat whose session has not connected, and for one whose CLI
    /// advertised none.
    ///
    /// The core retains them from the init frame each turn and from
    /// `commands_changed`, so a view that arrived after the turn started
    /// reads them here rather than waiting a whole turn for the next
    /// frame.
    pub fn slash_commands(&self, slot: &SessionSlot) -> Vec<AvailableCommand> {
        self.workspace.available_commands_for(slot)
    }

    /// The subagents the CLI last advertised for `slot`, retained by the
    /// same frames and empty in the same two cases as
    /// [`Self::slash_commands`].
    pub fn subagents(&self, slot: &SessionSlot) -> Vec<AvailableAgent> {
        self.workspace.available_agents_for(slot)
    }

    /// One working tree's files, walked on demand. `root` is the
    /// session's own scan cwd, which a git worker's worktree overrides.
    ///
    /// This walks the whole tree on the calling thread, the way
    /// [`Self::conversation`] reads a whole transcript: a caller offloads
    /// it rather than running it in a handler. Gitignore is respected,
    /// which is the default the TUI's own preference starts from.
    ///
    /// No `self`: nothing here is a fact about the core, so the two reads
    /// that need no session are reached at the surface rather than
    /// through an instance of it.
    pub fn file_index(root: &Path) -> FileIndex {
        FileIndex::scan(root, true)
    }

    /// The emoji a `:query` matches, best first, on the same terms as
    /// [`Self::file_index`]. A query below two characters matches
    /// nothing, which is what keeps `:D` and `10:30` from opening a
    /// picker.
    pub fn emoji(query: &str) -> Vec<&'static Emoji> {
        emoji::matches(query)
    }
}

#[cfg(test)]
mod tests {
    use forge_primitives::SessionSlot;

    use crate::surface::ViewSurface;

    fn slot() -> SessionSlot {
        SessionSlot::lead("TestOrg", "forge")
    }

    /// A seat whose CLI has advertised nothing reads as empty: a view
    /// opening on a session that has not connected renders an empty
    /// list, not an error and not another seat's.
    #[test]
    fn slash_commands_reads_the_seat_it_was_asked_about() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(std::sync::Arc::clone(&workspace));

        assert!(surface.slash_commands(&slot()).is_empty(), "nothing advertised reads as empty");

        workspace.seed_test_advertised_catalogues(
            &slot(),
            vec![forge_primitives::AvailableCommand::new("/help", "Open help")],
            Vec::new(),
        );

        let commands = surface.slash_commands(&slot());
        assert_eq!(commands.len(), 1, "the seat's own list reaches the view");
        assert_eq!(commands[0].name, "/help");
        assert!(
            surface.slash_commands(&SessionSlot::lead("TestOrg", "nothing")).is_empty(),
            "and a seat with no declaration reads as empty rather than sharing one",
        );
    }

    /// The agent catalogue, the same way.
    #[test]
    fn subagents_reads_the_seat_it_was_asked_about() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(std::sync::Arc::clone(&workspace));

        assert!(surface.subagents(&slot()).is_empty(), "nothing advertised reads as empty");

        workspace.seed_test_advertised_catalogues(
            &slot(),
            Vec::new(),
            vec![forge_primitives::AvailableAgent::new("reviewer", "Review code")],
        );

        let agents = surface.subagents(&slot());
        assert_eq!(agents.len(), 1, "the seat's own catalogue reaches the view");
        assert_eq!(agents[0].name, "reviewer");
    }

    /// The emoji table is view-side data, so the verb answers with no
    /// session at all, and it ranks the way the TUI's picker does.
    #[test]
    fn emoji_ranks_a_query_the_way_the_picker_does() {
        let ranked = ViewSurface::emoji("sm");

        assert_eq!(
            ranked.first().map(|e| e.name),
            Some("smile"),
            "the shortest prefix match leads: {:?}",
            ranked.iter().map(|e| e.name).collect::<Vec<_>>(),
        );
        assert!(ViewSurface::emoji("").is_empty(), "and a bare `:` holds nothing back");
    }

    /// The file index is walked on demand over the same walker the TUI
    /// streams from, rooted where the caller says.
    #[test]
    fn file_index_walks_the_root_it_is_given() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("src")).expect("mkdir");
        std::fs::write(root.path().join("src/main.rs"), "").expect("write");

        let index = ViewSurface::file_index(root.path());

        assert!(index.entries.contains_key("src/main.rs"), "the walk reaches the files");
        let ranked = index.visible("main", 10);
        assert_eq!(ranked.len(), 1, "and the verb's own ranking finds them: {ranked:?}");
        assert_eq!(ranked[0].rel_path, "src/main.rs");
    }
}
