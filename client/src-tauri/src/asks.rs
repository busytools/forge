//! Asks a person cannot see, decided in the shell's own half.
//!
//! **A parked page runs no timer.** A WKWebView with the display off consumes
//! its socket only while the page is awake, its timers stop with it, and a
//! frozen Android WebView or a hidden tab throttles to about a tick a minute -
//! so a notification rule living only in the page is dead exactly when a
//! notification matters. The bridge reads every frame whether or not the page
//! runs, so it decides here, over the same home roster the page reads and on
//! the same ask identity: a seat's pending kind and `pending_seq`.
//!
//! **The rule is the page's.** An ask is news when it arrives for a seat the
//! person is not looking at - not the shown seat, or the window is not
//! focused. A first roster read is the launch's own history and announces
//! nothing; only announced asks are marked, so one glanced at fires once the
//! person looks away; and an answer that shrinks a batch is not news.

use std::collections::{HashMap, HashSet};
#[cfg(mobile)]
use std::sync::atomic::{AtomicBool, Ordering};

use forge_primitives::SessionSlot;
use serde_json::Value;

/// Whether the app is in front, as the platform reports it.
///
/// **`window.is_focused()` is not that on Android**: tao's window answers
/// false there unconditionally, so the rule's "the seat on screen is not
/// news while it is looked at" could never hold on the phone. The app's own
/// resume and suspend events are the truth this side has.
#[cfg(mobile)]
static FOREGROUND: AtomicBool = AtomicBool::new(true);

/// Note whether the app came to the front or went behind.
#[cfg(mobile)]
pub(crate) fn set_foreground(foreground: bool) {
    FOREGROUND.store(foreground, Ordering::Relaxed);
}

/// Whether the app is in front, on a platform whose window cannot say.
#[cfg(target_os = "android")]
pub(crate) fn foreground() -> bool {
    FOREGROUND.load(Ordering::Relaxed)
}

/// The kind of ask a roster row reports, on the wire's own names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskKind {
    Question,
    Permission,
    HandOff,
}

impl AskKind {
    /// The ask in the OS notification's own words.
    pub fn body(self) -> &'static str {
        match self {
            Self::Question => "a question is waiting",
            Self::Permission => "a permission is waiting",
            Self::HandOff => "a handoff is waiting",
        }
    }
}

/// One seat's ask, as `AgentWire` writes it.
struct Asked {
    slot: SessionSlot,
    kind: AskKind,
    seq: u64,
}

/// One row's ask, or `None` when the row holds none.
///
/// **A row this build cannot read is skipped, not fatal.** A newer server's
/// ask kind is one row's news this build has no vocabulary for; failing the
/// array would silence every other seat's ask with it.
fn asked_of(row: &Value) -> Result<Option<Asked>, String> {
    let Some(slot) = row.get("slot") else {
        return Err("the row names no seat".to_owned());
    };
    let slot: SessionSlot = serde_json::from_value(slot.clone())
        .map_err(|why| format!("the row's seat is unreadable: {why}"))?;
    let pending = row.get("pending");
    let kind = match pending {
        None | Some(Value::Null) => return Ok(None),
        Some(held) => match held.as_str() {
            Some("question") => AskKind::Question,
            Some("permission") => AskKind::Permission,
            Some("hand_off") => AskKind::HandOff,
            other => {
                return Err(format!(
                    "{} names a pending kind this build does not know: {other:?}",
                    slot.display(),
                ));
            }
        },
    };
    let seq = row.get("pending_seq").and_then(Value::as_u64).unwrap_or(0);
    Ok(Some(Asked { slot, kind, seq }))
}

/// What has been announced, and whether a roster read has seeded it.
#[derive(Default)]
pub struct Asks {
    /// The server run an ask was announced in, its kind and its number.
    ///
    /// **The run scopes the number**: a forge restart starts the counter
    /// again, and a mark from a dead run must never match a live one.
    seen: HashMap<SessionSlot, (String, AskKind, u64)>,
    seeded: bool,
}

impl Asks {
    /// The asks one home read raises.
    ///
    /// A roster this half cannot read announces nothing and seeds nothing: a
    /// read that did not parse is not the launch's history.
    pub fn rising(
        &mut self,
        data: &Value,
        shown: Option<&SessionSlot>,
        focused: bool,
    ) -> Vec<(SessionSlot, AskKind)> {
        let Some(rows) = data.get("agents").and_then(Value::as_array) else {
            return Vec::new();
        };
        let boot = data.get("boot_id").and_then(Value::as_str).unwrap_or_default();
        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            match asked_of(row) {
                Ok(Some(agent)) => agents.push(agent),
                Ok(None) => {}
                Err(why) => tauri_plugin_log::log::debug!(
                    "a roster row could not be read for the ask rule: {why}",
                ),
            }
        }
        let first = !self.seeded;
        self.seeded = true;
        let mut notices = Vec::new();
        let mut live = HashSet::with_capacity(agents.len());
        for agent in &agents {
            let Asked { slot, kind, seq } = agent;
            live.insert(slot.clone());
            if self
                .seen
                .get(slot)
                .is_some_and(|(run, held, held_seq)| run == boot && held == kind && held_seq == seq)
            {
                continue;
            }
            let mark = (boot.to_owned(), *kind, *seq);
            if first {
                self.seen.insert(slot.clone(), mark);
                continue;
            }
            // **A seat on screen is not news while it is looked at**, and it
            // is not marked either: the read after the person looks away
            // announces it.
            if shown == Some(slot) && focused {
                continue;
            }
            self.seen.insert(slot.clone(), mark);
            notices.push((slot.clone(), *kind));
        }
        self.seen.retain(|slot, _| live.contains(slot));
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::{AskKind, Asks};
    use forge_primitives::SessionSlot;
    use serde_json::json;

    fn seat(label: &str) -> serde_json::Value {
        json!({ "org": "Busytools", "project": "forge", "label": label })
    }

    fn roster(rows: &[(&str, Option<&str>, u64)]) -> serde_json::Value {
        roster_of("run-1", rows)
    }

    fn roster_of(run: &str, rows: &[(&str, Option<&str>, u64)]) -> serde_json::Value {
        let agents: Vec<serde_json::Value> = rows
            .iter()
            .map(|(label, pending, seq)| {
                json!({
                    "slot": seat(label),
                    "pending": pending,
                    "pending_depth": 1,
                    "pending_seq": seq,
                })
            })
            .collect();
        json!({ "boot_id": run, "agents": agents })
    }

    fn lead() -> SessionSlot {
        SessionSlot::lead("Busytools", "forge")
    }

    fn worker() -> SessionSlot {
        SessionSlot::worker("Busytools", "forge", "w1")
    }

    /// **A first read is the launch's own history**, so a relaunch with a
    /// parked ask announces none of it.
    #[test]
    fn the_first_read_seeds_without_announcing() {
        let mut asks = Asks::default();
        let parked = roster(&[("lead", Some("permission"), 1), ("w1", Some("question"), 1)]);
        assert!(asks.rising(&parked, None, true).is_empty(), "launch is not news");
        assert!(
            asks.rising(&parked, None, true).is_empty(),
            "and the second read agrees with the seed",
        );
    }

    /// An ask on another seat than the one shown is news.
    #[test]
    fn another_seats_ask_is_news() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("question"), 2)]), Some(&lead()), true),
            vec![(worker(), AskKind::Question)],
        );
    }

    /// **The shown seat is announced once it is not looked at**, and the
    /// glance does not consume it.
    #[test]
    fn the_shown_seat_waits_for_the_person_to_look_away() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        let parked = roster(&[("lead", Some("permission"), 3)]);
        assert!(asks.rising(&parked, Some(&lead()), true).is_empty(), "looked at");
        assert!(asks.rising(&parked, Some(&lead()), true).is_empty(), "still looked at");
        assert_eq!(
            asks.rising(&parked, Some(&lead()), false),
            vec![(lead(), AskKind::Permission)],
            "the window lost focus, so it is news",
        );
    }

    /// **A re-read is not a new ask**, and an answer that shrinks the batch is
    /// not news either - the ask's sequence is the identity.
    #[test]
    fn an_answer_that_shrinks_the_batch_is_not_news() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        assert_eq!(asks.rising(&roster(&[("w1", Some("permission"), 4)]), None, true).len(), 1);
        assert!(
            asks.rising(&roster(&[("w1", Some("permission"), 4)]), None, true).is_empty(),
            "the same ask again",
        );
    }

    /// **An ask replacing another at the same depth is news**, which a
    /// kind-and-depth identity could not see.
    #[test]
    fn a_replaced_ask_at_the_same_depth_is_news() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        asks.rising(&roster(&[("w1", Some("question"), 4)]), None, true);
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("permission"), 5)]), None, true),
            vec![(worker(), AskKind::Permission)],
        );
    }

    /// **A forge restart starts the counter again, so the run scopes the
    /// mark.** A mark from the dead run cannot swallow this run's first ask.
    #[test]
    fn a_restart_starts_the_marks_over() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("question"), 1)]), None, true),
            vec![(worker(), AskKind::Question)],
        );
        assert!(
            asks.rising(&roster(&[("w1", Some("question"), 1)]), None, true).is_empty(),
            "the same run's same number is the same ask",
        );
        assert_eq!(
            asks.rising(&roster_of("run-2", &[("w1", Some("question"), 1)]), None, true),
            vec![(worker(), AskKind::Question)],
            "the next run's number 1 is a different ask",
        );
    }

    /// **One unreadable row does not silence the rest.** A newer server's ask
    /// kind is skipped and said in the log; the other seats still announce.
    #[test]
    fn an_unknown_ask_kind_skips_its_row_and_keeps_the_others() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        let rows = roster(&[("lead", Some("elicit"), 1), ("w1", Some("question"), 1)]);
        assert_eq!(
            asks.rising(&rows, None, true),
            vec![(worker(), AskKind::Question)],
            "the row this build cannot read is skipped, not the array",
        );
    }

    /// A roster this half cannot read is not the launch's history: it seeds
    /// nothing, so the first READABLE read is the one that seeds - and the
    /// ask after it is the first that can be news.
    #[test]
    fn an_unreadable_read_does_not_seed() {
        let mut asks = Asks::default();
        assert!(asks.rising(&json!({ "agents": "not a list" }), None, true).is_empty());
        assert!(
            asks.rising(&roster(&[("w1", Some("question"), 1)]), None, true).is_empty(),
            "the first readable read is the launch's history",
        );
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("question"), 2)]), None, true),
            vec![(worker(), AskKind::Question)],
            "the ask after it is news",
        );
    }
}
