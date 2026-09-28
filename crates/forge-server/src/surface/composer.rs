//! `slash_commands()`, `subagents()`, `forge_commands()`, `file_index()`,
//! `respect_gitignore()`: what the composer's autocomplete triggers read.
//!
//! Two of them are facts about a session - what the CLI advertised - so
//! the core holds them and these verbs read through it. The other three
//! have no session in them at all: forge's own command table and the file
//! walk are data a view reads, and the walk's ignore preference is the
//! user's own, which the core reads on the walk's behalf.
//!
//! The emoji set is not here. Which shortcodes exist and which a query
//! selects is the typeahead's own business, so it lives with the typeahead
//! rather than being handed to it.
//!
//! The tests here read the two session facts through a fixture that
//! writes the core's fields directly, so they pin the reads and not the
//! keeping: whether the init frame and `commands_changed` retain anything
//! at all is pinned by `forge-workspace`'s session-task tests.

use std::path::Path;

use forge_primitives::{AvailableAgent, AvailableCommand, SessionSlot};

use crate::commands::ForgeCommand;
use crate::file_index::FileIndex;
use crate::surface::ViewSurface;

impl ViewSurface {
    /// The slash commands the CLI last advertised for `slot`, named as
    /// they are typed. Empty for a seat whose session has not connected,
    /// and for one whose CLI advertised none.
    ///
    /// The core retains them from the init frame each turn and from
    /// `commands_changed`, so a view that arrived after the turn started
    /// reads them here rather than waiting a whole turn for the next
    /// frame.
    ///
    /// The wire carries the names bare, so the leading slash is added
    /// here: a caller renders `name` as it comes back.
    ///
    /// Forge's own commands are not in this list. A view rendering the `/`
    /// dropdown reads those from [`Self::forge_commands`], and a name in
    /// both is forge's, which handles it rather than forwarding it.
    pub fn slash_commands(&self, slot: &SessionSlot) -> Vec<AvailableCommand> {
        self.workspace
            .available_commands_for(slot)
            .into_iter()
            .map(|command| AvailableCommand {
                name: forge_workspace::translate::commands::slash_name(&command.name),
                ..command
            })
            .collect()
    }

    /// forge's own commands, which a view offers beside the ones the CLI
    /// advertises: a `/` dropdown is built from this table and
    /// [`Self::slash_commands`] together, so the two views cannot show
    /// different lists.
    ///
    /// A name here shadows the CLI's row for the same name
    /// (`crate::commands::is_forge_command`), because forge handles it
    /// rather than forwarding it.
    pub fn forge_commands() -> &'static [ForgeCommand] {
        crate::commands::FORGE_COMMANDS
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
    /// it rather than running it in a handler.
    ///
    /// The walk runs the way the user asked for it: gitignore is honoured
    /// unless the CLI's own `respectGitignore` preference turns it off.
    /// The preference is read here rather than parsed by a view, and read
    /// on each walk rather than held, so a flip reaches this read at once.
    /// The terminal answers from the same rule but on its own cadence: it
    /// re-reads the document when its settings reload, so a flip mid-run
    /// moves this list first and its own at the next reload.
    ///
    /// `self` is for that read alone: nothing else here is a fact about
    /// the core, so the command table is reached at the surface rather
    /// than through an instance of it.
    pub fn file_index(&self, root: &Path) -> FileIndex {
        FileIndex::scan(root, self.respect_gitignore())
    }

    /// The user's own `respectGitignore`, the preference
    /// [`Self::file_index`] walks with. Read on each call from the CLI's
    /// per-user preferences document, which is the core's document rather
    /// than a view's.
    ///
    /// A caller that caches a walk reads this to key its cache: the
    /// preference decides what the walk returns, so a cached walk is only
    /// an answer for the preference it was built under.
    pub fn respect_gitignore(&self) -> bool {
        let preferences = self.workspace.user_preferences();
        crate::file_index::respect_gitignore(preferences.as_ref())
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
            vec![forge_primitives::AvailableCommand::new("help", "Open help")],
            Vec::new(),
        );

        let commands = surface.slash_commands(&slot());
        assert_eq!(commands.len(), 1, "the seat's own list reaches the view");
        assert_eq!(
            commands[0].name, "/help",
            "and named as it is typed, which is not how the wire carries it",
        );
        assert_eq!(commands[0].description, "Open help", "the rest of the entry comes through");
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

    /// The file index is walked on demand over the same walker the TUI
    /// streams from, rooted where the caller says.
    #[test]
    fn file_index_walks_the_root_it_is_given() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("src")).expect("mkdir");
        std::fs::write(root.path().join("src/main.rs"), "").expect("write");
        let (workspace, _dir) = crate::surface::testing::workspace();
        // The walk's ignore preference comes from the CLI's per-user
        // document, which this fixture holds rather than the machine's.
        workspace.seed_test_user_preferences(serde_json::json!({}));

        let index = ViewSurface::new(std::sync::Arc::clone(&workspace)).file_index(root.path());

        assert!(index.entries.contains_key("src/main.rs"), "the walk reaches the files");
        let ranked = index.visible("main", 10);
        assert_eq!(ranked.len(), 1, "and the verb's own ranking finds them: {ranked:?}");
        assert_eq!(ranked[0].rel_path, "src/main.rs");
    }

    /// The preference is read on each walk rather than held: the user can
    /// flip it while forge runs, and the walk a view asks for next has to
    /// answer the new one rather than the one forge started with.
    #[test]
    fn file_index_walks_under_the_preference_read_at_that_call() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join(".git")).expect("mkdir");
        std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(root.path().join("ignored.rs"), "").expect("write");
        let (workspace, _dir) = crate::surface::testing::workspace();
        workspace.seed_test_user_preferences(serde_json::json!({}));
        let surface = ViewSurface::new(std::sync::Arc::clone(&workspace));

        assert!(
            !surface.file_index(root.path()).entries.contains_key("ignored.rs"),
            "the walk respects the file while the preference says so",
        );

        workspace.seed_test_user_preferences(serde_json::json!({"respectGitignore": false}));

        assert!(
            surface.file_index(root.path()).entries.contains_key("ignored.rs"),
            "and takes the preference read at the next call",
        );
    }
}
