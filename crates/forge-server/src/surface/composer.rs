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

    /// Whether the conversation at `slot` dispatched a sub-agent.
    ///
    /// A fact about the whole conversation rather than about a window of it,
    /// and one the session task's fold raises and announces - so this answers
    /// what that fold holds rather than scanning the messages again here.
    pub fn has_dispatches(&self, slot: &SessionSlot) -> bool {
        self.workspace.has_dispatches_for(slot)
    }

    /// The seat's file index, as the seat's own loop last walked it, or
    /// `None` until the first walk lands.
    ///
    /// **Held rather than walked here.** The walk is a whole tree, and the
    /// seat's loop is the one that takes it - throttled, and announced as
    /// `FileIndexChanged` when it moves - so this answers the store the loop
    /// writes rather than paying for a walk of its own.
    pub fn file_index(&self, slot: &SessionSlot) -> Option<std::sync::Arc<FileIndex>> {
        self.workspace.file_index(slot).map(|held| held.index)
    }

    /// One working tree's files, walked on demand. `root` is the session's
    /// own scan cwd, which a git worker's worktree overrides.
    ///
    /// **The parked web view's path.** A page that holds no seat - forge-web
    /// serves from the process and subscribes to nothing, so no seat's loop
    /// runs for it - has no store to read, and walks where it stands.
    pub fn walk_file_index(&self, root: &std::path::Path) -> FileIndex {
        FileIndex::scan(root, self.respect_gitignore())
    }

    /// The user's own `respectGitignore`, the preference
    /// [`Self::walk_file_index`] runs with. Read on each call from the CLI's
    /// per-user preferences document, so a flip reaches the next walk.
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

    /// The index is the seat's own: a slot nothing has walked reads as
    /// `None` rather than paying for a walk of the tree here, and what the
    /// seat's loop stores is what this answers.
    ///
    /// The walk itself is the workspace's (`a_walk_happens_only_once_the_
    /// window_has_passed` and its siblings, beside the seat's loop).
    #[tokio::test]
    async fn the_file_index_answers_the_seat_that_was_walked() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(std::sync::Arc::clone(&workspace));
        let seat = slot();
        workspace.register_domain_session(seat.clone(), None);

        assert!(surface.file_index(&seat).is_none(), "a slot nothing walked reads as none");

        let mut index = forge_workspace::file_index::FileIndex::default();
        index.entries.insert(
            "src/main.rs".to_owned(),
            forge_workspace::file_index::FileCandidate {
                rel_path: "src/main.rs".to_owned(),
                rel_path_lower: "src/main.rs".to_owned(),
                basename_lower: "main.rs".to_owned(),
                depth: 1,
            },
        );
        workspace.store_file_index(&seat, std::sync::Arc::new(index));

        let held = surface.file_index(&seat).expect("the walk the seat holds comes back");
        assert!(held.entries.contains_key("src/main.rs"), "with the files the walk found");
        let other = SessionSlot::worker("TestOrg", "forge", "worker");
        assert!(surface.file_index(&other).is_none(), "and it is the seat's own, not another's");
    }
}
