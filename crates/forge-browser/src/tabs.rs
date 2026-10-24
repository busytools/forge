//! The Playwright driver's tab list, as the service reads it.
//!
//! **Upstream's list is positional and renumbers on every close**; forge
//! needs a stable name per tab, and the patched driver appends one line with
//! each tab's CDP target id. The target id is the identity: it survives a
//! renumber, it is what the host's frames and the CDP input target already
//! speak, and the service maps it to its own small tab number.

use std::collections::{BTreeMap, HashMap, HashSet};

use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserTabRow, BrowserTabState};
use serde::Deserialize;

/// One tab as the driver lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DriverTab {
    #[serde(rename = "i")]
    pub index: usize,
    #[serde(rename = "id")]
    pub target: String,
    pub title: String,
    pub url: String,
    pub current: bool,
}

const EXPORT: &str = "FORGE-TABS ";

/// The export line's rows, or why the listing cannot be trusted.
pub fn parse_export(text: &str) -> Result<Vec<DriverTab>, String> {
    let line = text.lines().find(|line| line.starts_with(EXPORT)).ok_or_else(|| {
        "the driver's tab list carried no FORGE-TABS export, so no tab can be named".to_owned()
    })?;
    serde_json::from_str(&line[EXPORT.len()..])
        .map_err(|why| format!("the driver's tab export did not parse: {why}"))
}

/// The listing with the export line removed.
pub fn strip_export(text: &str) -> String {
    let mut listing = String::new();
    for line in text.lines().filter(|line| !line.starts_with(EXPORT)) {
        listing.push_str(line);
        listing.push('\n');
    }
    listing
}

/// The Playwright index of the tab a target id names.
pub fn index_of(tabs: &[DriverTab], target: &str) -> Option<usize> {
    tabs.iter().find(|tab| tab.target == target).map(|tab| tab.index)
}

/// The profile's tabs, keyed by the host's own tab number. **The registry
/// never mints one**: the host's numbers are what frames, mouse input and
/// `TabClosed` already carry, and a second counter would drift from them.
#[derive(Default)]
pub struct TabRegistry {
    tabs: BTreeMap<u16, Tab>,
    current: HashMap<SessionSlot, u16>,
    /// Every number this registry has ever taken. **A retired number is
    /// never handed out again**, so a hand-off written against it can never
    /// come to mean another tab.
    retired: HashSet<u16>,
}

pub(crate) struct Tab {
    pub(crate) owner: SessionSlot,
    pub(crate) target: String,
    title: String,
    url: String,
    busy: bool,
}

impl TabRegistry {
    /// Take the host's tab `number` for `owner`. A number the registry has
    /// seen before is a refusal: the host never reuses one, so a duplicate
    /// is a bug in the pairing, not a tab to overwrite.
    pub fn adopt(
        &mut self,
        number: u16,
        owner: SessionSlot,
        target: &str,
        title: &str,
        url: &str,
    ) -> Result<u16, String> {
        if self.tabs.contains_key(&number) {
            return Err(format!("tab {number} is already open"));
        }
        if self.retired.contains(&number) {
            return Err(format!("tab {number} was already used by this host"));
        }
        self.retired.insert(number);
        self.current.insert(owner.clone(), number);
        self.tabs.insert(
            number,
            Tab {
                owner,
                target: target.to_owned(),
                title: title.to_owned(),
                url: url.to_owned(),
                busy: false,
            },
        );
        Ok(number)
    }

    /// A tab the page opened: it belongs to whoever owns `opener`.
    pub fn create_under(
        &mut self,
        opener: u16,
        number: u16,
        target: &str,
        title: &str,
        url: &str,
    ) -> Result<u16, String> {
        let owner = self
            .owner(opener)
            .cloned()
            .ok_or_else(|| format!("the popup's opener tab {opener} is not open"))?;
        self.adopt(number, owner, target, title, url)
    }

    /// Remove one tab and answer the session that owned it.
    pub fn close(&mut self, tab: u16) -> Option<SessionSlot> {
        let removed = self.tabs.remove(&tab)?;
        self.current.retain(|_, number| *number != tab);
        Some(removed.owner)
    }

    pub fn close_owned(
        &mut self,
        session: &SessionSlot,
        tab: u16,
    ) -> Result<Option<SessionSlot>, String> {
        self.owned(session, tab)?;
        Ok(self.close(tab))
    }

    pub fn current(&self, session: &SessionSlot) -> Option<u16> {
        self.current.get(session).copied().filter(|tab| self.tabs.contains_key(tab))
    }

    /// Forget a session's current tab without closing it: a replacement
    /// keeps the slot but inherits none of the occupant's place.
    pub fn forget_current(&mut self, session: &SessionSlot) {
        self.current.remove(session);
    }

    pub fn select(&mut self, session: &SessionSlot, tab: u16) -> Result<(), String> {
        self.owned(session, tab)?;
        self.current.insert(session.clone(), tab);
        Ok(())
    }

    pub fn number_of(&self, target: &str) -> Option<u16> {
        self.tabs.iter().find(|(_, tab)| tab.target == target).map(|(number, _)| *number)
    }

    pub fn target(&self, tab: u16) -> Option<&str> {
        self.tabs.get(&tab).map(|tab| tab.target.as_str())
    }

    pub fn owner(&self, tab: u16) -> Option<&SessionSlot> {
        self.tabs.get(&tab).map(|tab| &tab.owner)
    }

    pub fn mark_busy(&mut self, tab: u16, busy: bool) {
        if let Some(entry) = self.tabs.get_mut(&tab) {
            entry.busy = busy;
        }
    }

    pub fn update(&mut self, tab: u16, title: &str, url: &str) {
        if let Some(entry) = self.tabs.get_mut(&tab) {
            title.clone_into(&mut entry.title);
            url.clone_into(&mut entry.url);
        }
    }

    pub(crate) fn owned(&self, session: &SessionSlot, tab: u16) -> Result<&Tab, String> {
        let entry = self.tabs.get(&tab).ok_or_else(|| format!("tab {tab} is not open"))?;
        if &entry.owner != session {
            return Err(format!("tab {tab} belongs to {}", entry.owner.display()));
        }
        Ok(entry)
    }

    /// The rows the strip draws. **A held tab outranks a busy one**: the
    /// person is being asked about it, which is the fact worth seeing.
    pub fn rows(&self, held: &HashSet<u16>) -> Vec<BrowserTabRow> {
        self.tabs
            .iter()
            .map(|(number, tab)| BrowserTabRow {
                number: *number,
                title: tab.title.clone(),
                url: tab.url.clone(),
                owner: tab.owner.clone(),
                state: if held.contains(number) {
                    BrowserTabState::Held
                } else if tab.busy {
                    BrowserTabState::InUse
                } else {
                    BrowserTabState::Idle
                },
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn numbers(&self) -> Vec<u16> {
        self.tabs.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{TabRegistry, parse_export, strip_export};
    use forge_primitives::SessionSlot;
    use forge_primitives::browser::BrowserTabState;
    use std::collections::HashSet;

    #[test]
    fn the_hosts_number_is_the_tabs_name() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let one = registry.adopt(1, seat.clone(), "A", "one", "http://a/").expect("the first tab");
        let two = registry.adopt(2, seat.clone(), "B", "two", "http://b/").expect("the second tab");
        assert_eq!((one, two), (1, 2));
        assert!(
            registry.adopt(2, seat.clone(), "C", "three", "http://c/").is_err(),
            "a number is never reused",
        );
        registry.close(one);
        assert_eq!(registry.number_of("B"), Some(two), "a target keeps its number");
        assert!(
            registry.adopt(1, seat, "D", "four", "http://d/").is_err(),
            "a closed number stays retired",
        );
    }

    #[test]
    fn only_an_owner_selects_or_closes_its_tab() {
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let mine = registry.adopt(1, lead.clone(), "A", "one", "u").expect("the tab");
        assert_eq!(registry.current(&lead), Some(mine), "a new tab is its owner's current");
        assert!(registry.select(&worker, mine).is_err(), "a foreign tab is refused");
        assert!(registry.close_owned(&worker, mine).is_err(), "and not closed");
        registry.select(&lead, mine).expect("its owner selects it");
        assert_eq!(registry.current(&lead), Some(mine));
        registry.close(mine);
        assert_eq!(registry.current(&lead), None, "closing the current tab clears the pointer");
    }

    #[test]
    fn rows_say_who_owns_each_tab_and_what_state_it_is_in() {
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let held = registry.adopt(1, lead.clone(), "A", "one", "http://a/").expect("the held tab");
        let busy =
            registry.adopt(2, worker.clone(), "B", "two", "http://b/").expect("the busy tab");
        registry.mark_busy(busy, true);
        let rows = registry.rows(&HashSet::from([held]));
        assert_eq!(rows[0].state, BrowserTabState::Held);
        assert_eq!(rows[1].state, BrowserTabState::InUse);
        assert_eq!(rows[0].owner, lead);
        assert_eq!(rows[1].owner, worker);

        // A neighbour's close moves no number and leaves the held tab held.
        registry.close(busy);
        let rows = registry.rows(&HashSet::from([held]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, held, "the held tab keeps the number the person was shown");
        assert_eq!(rows[0].state, BrowserTabState::Held);
    }

    #[test]
    fn a_popup_belongs_to_its_openers_owner() {
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let opener = registry.adopt(1, worker.clone(), "A", "one", "u").expect("the opener");
        let popup =
            registry.create_under(opener, 2, "B", "two", "http://b/").expect("the opener exists");
        assert_eq!(
            registry.owner(popup),
            Some(&worker),
            "the popup is the opener's, not the caller's"
        );
        assert!(
            registry.create_under(99, 3, "C", "three", "http://c/").is_err(),
            "an unknown opener is refused",
        );
    }

    /// **The export is Playwright's own line plus one machine line.** The
    /// agent never sees it: the service re-renders the list in forge's words.
    #[test]
    fn the_export_names_each_tab_target_and_position() {
        let text = "FORGE-TABS [{\"i\":0,\"id\":\"ABC\",\"title\":\"one\",\"url\":\"http://a/\",\
                    \"current\":true},{\"i\":1,\"id\":\"DEF\",\"title\":\"two\",\"url\":\"http://b/\",\
                    \"current\":false}]\n- 0: (current) [one](http://a/)\n- 1: [two](http://b/)\n";
        let tabs = parse_export(text).expect("the export parses");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].index, 1);
        assert_eq!(tabs[1].target, "DEF");
        assert_eq!(tabs[1].title, "two");
        assert!(tabs[0].current, "the first tab is the current one");
        assert!(
            parse_export("- 0: [one](http://a/)\n").is_err(),
            "a listing with no export is refused"
        );
    }

    /// **The line and its raw text do not both reach the agent.** The caller
    /// drops the export before the parts are returned.
    #[test]
    fn the_export_is_stripped_from_the_agent_visible_text() {
        let text = "FORGE-TABS []\n- 0: (current) [one](http://a/)\n";
        assert_eq!(strip_export(text), "- 0: (current) [one](http://a/)\n");
    }
}
