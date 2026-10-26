//! The Playwright driver's tab list, as the service reads it.
//!
//! **Upstream's list is positional and renumbers on every close**, and CEF
//! swaps a page's CDP target id when it navigates. The patched driver appends
//! one line naming each tab's page guid (the stable identity) and its target
//! id right now (where CDP drives it); the service keys its own small tab
//! numbers by the guid and refreshes the target from every listing.

use std::collections::{BTreeMap, HashMap, HashSet};

use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserTabRow, BrowserTabState};
use serde::Deserialize;

/// One tab as the driver lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DriverTab {
    #[serde(rename = "i")]
    pub index: usize,
    /// The page guid: the one name that survives a navigation.
    pub id: String,
    /// The page's CDP target id right now; CEF swaps it on navigation.
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

/// The Playwright index of the tab a page guid names. **An empty name names
/// nothing**, the same refusal [`index_of_target`] makes for an empty target.
pub fn index_of(tabs: &[DriverTab], page: &str) -> Option<usize> {
    if page.is_empty() {
        return None;
    }
    tabs.iter().find(|tab| tab.id == page).map(|tab| tab.index)
}

/// The Playwright index of the page a CDP target names right now.
pub fn index_of_target(tabs: &[DriverTab], target: &str) -> Option<usize> {
    if target.is_empty() {
        return None;
    }
    tabs.iter().find(|tab| tab.target == target).map(|tab| tab.index)
}

/// The pages the driver lists that no tab claims. **A page the driver made
/// for itself is not a host tab**, and it is closed by its target so the
/// listing cannot grow pages nothing paints.
pub fn orphan_targets(tabs: &TabRegistry, listing: &[DriverTab]) -> Vec<String> {
    let known: HashSet<String> = tabs
        .numbers()
        .into_iter()
        .filter_map(|number| tabs.target(number).map(str::to_owned))
        .collect();
    listing
        .iter()
        .filter(|row| !row.target.is_empty() && !known.contains(&row.target))
        .map(|row| row.target.clone())
        .collect()
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
    /// Tabs whose page a listing could not name. **Their pages are not
    /// orphans**: the prune closes nothing while one is unresolved, or a live
    /// document would be closed because its row could not be paired.
    unresolved: HashSet<u16>,
}

pub(crate) struct Tab {
    pub(crate) owner: SessionSlot,
    /// The page guid the driver names the tab by; stable across navigation.
    pub(crate) id: String,
    /// The page's current CDP target id; CEF swaps it on navigation.
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
        id: &str,
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
                id: id.to_owned(),
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
        id: &str,
        target: &str,
        title: &str,
        url: &str,
    ) -> Result<u16, String> {
        let owner = self
            .owner(opener)
            .cloned()
            .ok_or_else(|| format!("the popup's opener tab {opener} is not open"))?;
        self.adopt(number, owner, id, target, title, url)
    }

    /// Remove one tab and answer the session that owned it. **The unresolved
    /// flag goes even for a row already gone**: a close landing between a
    /// listing and its mark must not leave the flag on a retired number.
    pub fn close(&mut self, tab: u16) -> Option<SessionSlot> {
        self.unresolved.remove(&tab);
        let removed = self.tabs.remove(&tab)?;
        self.current.retain(|_, number| *number != tab);
        Some(removed.owner)
    }

    /// Remember that a listing could not name this tab's page, so no page is
    /// pruned as an orphan while the row is unresolved. **Only an open row is
    /// marked**: a number with no row has no page to protect.
    pub fn mark_unresolved(&mut self, tab: u16) {
        if self.tabs.contains_key(&tab) {
            self.unresolved.insert(tab);
        }
    }

    /// The tabs whose page a listing could not name.
    pub fn unresolved_tabs(&self) -> Vec<u16> {
        self.unresolved.iter().copied().collect()
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

    pub fn number_of(&self, page: &str) -> Option<u16> {
        self.tabs.iter().find(|(_, tab)| tab.id == page).map(|(number, _)| *number)
    }

    /// The page guid the driver names this tab by.
    pub fn id(&self, tab: u16) -> Option<&str> {
        self.tabs.get(&tab).map(|tab| tab.id.as_str())
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

    /// **A navigation can swap the page target id.** CEF gives a navigated
    /// page a new DevTools target, while the driver's Tab survives it; the
    /// registry follows the driver's row so the next call selects the page
    /// that is really there.
    pub fn retarget(&mut self, tab: u16, target: &str) {
        if let Some(entry) = self.tabs.get_mut(&tab) {
            target.clone_into(&mut entry.target);
        }
        self.unresolved.remove(&tab);
    }

    /// Re-pair one row with the name a fresh driver gives its page. **A
    /// driver replacement renames every page** - the guid belongs to the
    /// driver's connection - while the CDP target is the browser's and
    /// survives it. The number stays this registry's.
    pub fn reidentify(&mut self, tab: u16, id: &str) {
        if let Some(entry) = self.tabs.get_mut(&tab) {
            id.clone_into(&mut entry.id);
        }
        self.unresolved.remove(&tab);
    }

    /// Re-pair every row the fresh listing names by its target, answering how
    /// many changed. A row whose target the listing does not carry is left
    /// alone: its page may be mid-navigation, and inventing a name for it is
    /// how a tab comes to mean another page. **Rows sharing a target are not
    /// renamed here**: the repair refuses to write one guid onto two rows.
    /// (The call and watch paths can still resolve such a target; this only
    /// keeps the repair from choosing for them.) The pairing goes through
    /// [`Self::reidentify`], so a re-paired row is no longer unresolved and
    /// the orphan prune runs again.
    pub fn reidentify_from(&mut self, listing: &[DriverTab]) -> usize {
        let mut sharing: HashMap<&str, usize> = HashMap::new();
        for tab in self.tabs.values() {
            if !tab.target.is_empty() {
                *sharing.entry(tab.target.as_str()).or_default() += 1;
            }
        }
        let duplicated: HashSet<String> = sharing
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(target, _)| target.to_owned())
            .collect();
        if !duplicated.is_empty() {
            tracing::debug!(
                event_name = "browser_tab_target_duplicated",
                targets = ?duplicated,
                "rows share a CDP target; the repair renames none of them"
            );
        }
        let mut changed = 0;
        for number in self.tabs.keys().copied().collect::<Vec<_>>() {
            let Some(tab) = self.tabs.get(&number) else { continue };
            if tab.target.is_empty() || duplicated.contains(tab.target.as_str()) {
                continue;
            }
            let Some(row) = listing.iter().find(|row| row.target == tab.target) else {
                continue;
            };
            if row.id != tab.id {
                self.reidentify(number, &row.id);
                changed += 1;
            }
        }
        changed
    }

    pub(crate) fn owned(&self, session: &SessionSlot, tab: u16) -> Result<&Tab, String> {
        let entry = self.tabs.get(&tab).ok_or_else(|| format!("tab {tab} is not open"))?;
        if &entry.owner != session {
            return Err(format!("tab {tab} belongs to {}", entry.owner.display()));
        }
        Ok(entry)
    }

    /// The rows the strip draws. **A held tab outranks a busy one**: the
    /// person is being asked about it, which is the fact worth seeing. **A
    /// tab a person is driving is in use**, like one a call is driving: the
    /// spec's in-use is either.
    pub fn rows(&self, held: &HashSet<u16>, controlled: &HashSet<u16>) -> Vec<BrowserTabRow> {
        self.tabs
            .iter()
            .map(|(number, tab)| BrowserTabRow {
                number: *number,
                title: tab.title.clone(),
                url: tab.url.clone(),
                owner: tab.owner.clone(),
                state: if held.contains(number) {
                    BrowserTabState::Held
                } else if tab.busy || controlled.contains(number) {
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
    use super::{DriverTab, TabRegistry, orphan_targets, parse_export, strip_export};
    use forge_primitives::SessionSlot;
    use forge_primitives::browser::BrowserTabState;
    use std::collections::HashSet;

    #[test]
    fn the_hosts_number_is_the_tabs_name() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let one =
            registry.adopt(1, seat.clone(), "p1", "t1", "one", "http://a/").expect("the first tab");
        let two = registry
            .adopt(2, seat.clone(), "p2", "t2", "two", "http://b/")
            .expect("the second tab");
        assert_eq!((one, two), (1, 2));
        assert!(
            registry.adopt(2, seat.clone(), "p3", "t3", "three", "http://c/").is_err(),
            "a number is never reused",
        );
        registry.close(one);
        assert_eq!(registry.number_of("p2"), Some(two), "a page keeps its number");
        assert!(
            registry.adopt(1, seat, "p4", "t4", "four", "http://d/").is_err(),
            "a closed number stays retired",
        );
    }

    #[test]
    fn only_an_owner_selects_or_closes_its_tab() {
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let mine = registry.adopt(1, lead.clone(), "p1", "t1", "one", "u").expect("the tab");
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
        let held =
            registry.adopt(1, lead.clone(), "p1", "t1", "one", "http://a/").expect("the held tab");
        let busy = registry
            .adopt(2, worker.clone(), "p2", "t2", "two", "http://b/")
            .expect("the busy tab");
        registry.mark_busy(busy, true);
        let rows = registry.rows(&HashSet::from([held]), &HashSet::new());
        assert_eq!(rows[0].state, BrowserTabState::Held);
        assert_eq!(rows[1].state, BrowserTabState::InUse);
        assert_eq!(rows[0].owner, lead);
        assert_eq!(rows[1].owner, worker);

        // **A person's control is a use too.** A tab nobody's call is driving
        // reads in use while a viewer holds its role.
        let driven = registry
            .adopt(3, worker.clone(), "p3", "t3", "three", "http://c/")
            .expect("the driven tab");
        let rows = registry.rows(&HashSet::from([held]), &HashSet::from([driven]));
        assert_eq!(rows[2].state, BrowserTabState::InUse, "the controlled tab is in use");

        // A neighbour's close moves no number and leaves the held tab held.
        registry.close(busy);
        registry.close(driven);
        let rows = registry.rows(&HashSet::from([held]), &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, held, "the held tab keeps the number the person was shown");
        assert_eq!(rows[0].state, BrowserTabState::Held);
    }

    /// **A page the driver made for itself is an orphan; a host tab is
    /// not.** The orphan is closed by target, and a row whose target the
    /// driver could not read is left alone rather than closed blind.
    #[test]
    fn only_a_driver_made_page_is_an_orphan() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        registry.adopt(1, seat, "p1", "T1", "one", "http://a/").expect("the host tab");
        let listing = [
            DriverTab {
                index: 0,
                id: "p1".into(),
                target: "T1".into(),
                title: String::new(),
                url: String::new(),
                current: false,
            },
            DriverTab {
                index: 1,
                id: "p2".into(),
                target: "T2".into(),
                title: String::new(),
                url: String::new(),
                current: true,
            },
            DriverTab {
                index: 2,
                id: "p3".into(),
                target: String::new(),
                title: String::new(),
                url: String::new(),
                current: false,
            },
        ];
        assert_eq!(orphan_targets(&registry, &listing), vec!["T2".to_owned()]);
    }

    #[test]
    fn a_navigation_retargets_the_tabs_page() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let tab = registry
            .adopt(1, seat.clone(), "page1", "old-target", "", "about:blank")
            .expect("the tab");
        registry.retarget(tab, "new-target");
        assert_eq!(registry.target(tab), Some("new-target"), "the CDP target follows the page");
        assert_eq!(registry.number_of("page1"), Some(tab), "the page guid is the stable name");
        assert_eq!(registry.number_of("old-target"), None, "a test does not need the target");
        assert_eq!(registry.owner(tab), Some(&seat), "the number and owner stay");
    }

    /// **A fresh driver renames every page; the target is what survives it.**
    /// Each row is re-paired by its target, a page the listing does not name
    /// keeps its old name, and the number stays the registry's.
    #[test]
    fn a_replaced_driver_is_re_paired_by_target() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let one = registry
            .adopt(1, seat.clone(), "old-1", "T1", "one", "http://a/")
            .expect("the first tab");
        let two = registry
            .adopt(2, seat.clone(), "old-2", "T2", "two", "http://b/")
            .expect("the second tab");
        let listing = [tab_row(0, "new-1", "T1"), tab_row(1, "new-2", "T2")];

        assert_eq!(super::index_of_target(&listing, "T2"), Some(1), "the target names the index");
        assert_eq!(super::index_of_target(&listing, ""), None, "an unread target names nothing");
        assert_eq!(registry.reidentify_from(&listing), 2, "both rows follow their targets");
        assert_eq!(registry.id(one), Some("new-1"));
        assert_eq!(registry.id(two), Some("new-2"));
        assert_eq!(registry.number_of("new-1"), Some(one), "the page keeps its number");

        // A page the fresh listing does not name is left alone: inventing a
        // name for it is how a tab comes to mean another page.
        let missing = [tab_row(0, "new-1", "T1")];
        assert_eq!(registry.reidentify_from(&missing), 0);
        assert_eq!(registry.id(two), Some("new-2"), "the unnamed row keeps its name");

        // **An empty guid names nothing**, the same refusal an empty target
        // makes; a page that names nothing must not select the first row.
        assert_eq!(super::index_of(&listing, ""), None, "an empty guid names no page");

        // Two rows sharing a target are left alone: naming both after one
        // page would let either number drive it.
        let mut shared = TabRegistry::default();
        let one = shared
            .adopt(1, seat.clone(), "s1", "TS", "one", "http://a/")
            .expect("the first shared row");
        let two = shared
            .adopt(2, seat.clone(), "s2", "TS", "two", "http://b/")
            .expect("the second shared row");
        assert_eq!(
            shared.reidentify_from(&[tab_row(0, "fresh", "TS")]),
            0,
            "a target two rows share re-pairs nothing",
        );
        assert_eq!(shared.id(one), Some("s1"));
        assert_eq!(shared.id(two), Some("s2"));
    }

    /// **A close clears the unresolved flag, and a number with no row is
    /// never marked**: a close landing between a listing and its mark must
    /// not hold the prune on a retired number.
    #[test]
    fn an_unresolved_flag_goes_with_its_row() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let one = registry.adopt(1, seat, "p1", "T1", "one", "u").expect("the tab");
        registry.mark_unresolved(one);
        assert_eq!(registry.unresolved_tabs(), vec![one]);
        registry.close(one);
        assert!(registry.unresolved_tabs().is_empty(), "a close clears the flag");

        registry.mark_unresolved(9);
        assert!(registry.unresolved_tabs().is_empty(), "a number with no row is not marked");
        registry.mark_unresolved(one);
        assert!(registry.unresolved_tabs().is_empty(), "a retired number is never marked");
    }

    fn tab_row(index: usize, id: &str, target: &str) -> DriverTab {
        DriverTab {
            index,
            id: id.to_owned(),
            target: target.to_owned(),
            title: String::new(),
            url: String::new(),
            current: false,
        }
    }

    #[test]
    fn a_popup_belongs_to_its_openers_owner() {
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let opener = registry.adopt(1, worker.clone(), "p1", "t1", "one", "u").expect("the opener");
        let popup = registry
            .create_under(opener, 2, "p2", "t2", "two", "http://b/")
            .expect("the opener exists");
        assert_eq!(
            registry.owner(popup),
            Some(&worker),
            "the popup is the opener's, not the caller's"
        );
        assert!(
            registry.create_under(99, 3, "p3", "t3", "three", "http://c/").is_err(),
            "an unknown opener is refused",
        );
    }

    /// **The export is Playwright's own line plus one machine line.** The
    /// agent never sees it: the service re-renders the list in forge's words.
    #[test]
    fn the_export_names_each_tab_target_and_position() {
        let text = "FORGE-TABS [{\"i\":0,\"id\":\"page@1\",\"target\":\"T1\",\"title\":\"one\",\
                    \"url\":\"http://a/\",\"current\":true},{\"i\":1,\"id\":\"page@2\",\
                    \"target\":\"T2\",\"title\":\"two\",\"url\":\"http://b/\",\"current\":false}]\n\
                    - 0: (current) [one](http://a/)\n- 1: [two](http://b/)\n";
        let tabs = parse_export(text).expect("the export parses");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].index, 1);
        assert_eq!(tabs[1].id, "page@2", "the guid is the stable name");
        assert_eq!(tabs[1].target, "T2", "and the target is where CDP drives it now");
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

    /// **A page is found by its guid, never by the target a navigation
    /// swapped.** A dead-page retry that looked up the old target found
    /// nothing and drove whatever page the fresh driver called current.
    #[test]
    fn a_page_is_found_by_its_guid_after_the_target_moved() {
        let listing = [super::DriverTab {
            index: 3,
            id: "page@1".to_owned(),
            target: "T-after".to_owned(),
            title: "one".to_owned(),
            url: "http://a/".to_owned(),
            current: false,
        }];
        assert_eq!(super::index_of(&listing, "page@1"), Some(3), "the guid finds the page");
        assert_eq!(super::index_of(&listing, "T-before"), None, "the old target is gone");
        assert_eq!(super::index_of(&listing, "T-after"), None, "the target is not the identity");
    }
}
