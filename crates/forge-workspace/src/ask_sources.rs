//! The workspace's own ask sources: one adapter per registry that still
//! lives here.
//!
//! The host keeps **one registered list** of ask sources: these adapters
//! plus whatever a moved subsystem registers (the vault family's registry).
//! `pending_asks`, `pending_interaction` and `pending_interaction_depth` all
//! walk that list, so a source cannot be counted without being drawn or
//! drawn without being counted - and the guard test parks one of each kind
//! its sources name, so a new kind cannot be added without a source that
//! raises it and a park that proves it.
//!
//! The session's own pending set (a question and a permission prompt) is not
//! a registry and is not here: it lives on the seat's `DomainSession`, and
//! the host reads it directly.

use std::collections::HashMap;
use std::sync::Arc;

use forge_primitives::{Ask, AskSource, PendingAsk, PendingInteractionKind, SessionSlot};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::documents::ParkedDocument;
use crate::settings_ask::ParkedSettingsWrite;
use crate::workspace::{ParkedBrowserHandOff, ParkedSlackDraft, ParkedTerminalHandOff};

/// The core's one registered ask-source list, in the order a dock draws
/// them.
///
/// **Both constructors call this.** A list maintained in two places is how
/// a source added to one and not the other keeps the guard green while the
/// production read never counts it - #2954's failure through another door.
pub(crate) fn registered(
    slack_drafts: &Arc<Mutex<HashMap<Uuid, ParkedSlackDraft>>>,
    browser_handoffs: &Arc<Mutex<HashMap<Uuid, ParkedBrowserHandOff>>>,
    terminal_handoffs: &Arc<Mutex<HashMap<Uuid, ParkedTerminalHandOff>>>,
    document_handoffs: &Arc<Mutex<HashMap<Uuid, ParkedDocument>>>,
    vault_asks: &Arc<forge_tools::vault::VaultAsks>,
    settings_writes: &Arc<Mutex<HashMap<Uuid, ParkedSettingsWrite>>>,
) -> Vec<Arc<dyn AskSource>> {
    vec![
        Arc::new(SlackDrafts(Arc::clone(slack_drafts))),
        Arc::new(BrowserHandOffs(Arc::clone(browser_handoffs))),
        Arc::new(TerminalHandOffs(Arc::clone(terminal_handoffs))),
        Arc::new(DocumentHandOffs(Arc::clone(document_handoffs))),
        vault_asks.clone(),
        Arc::new(SettingsWrites(Arc::clone(settings_writes))),
    ]
}

/// Composed Slack messages held for a decision.
pub(crate) struct SlackDrafts(pub Arc<Mutex<HashMap<Uuid, ParkedSlackDraft>>>);

impl AskSource for SlackDrafts {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.0
            .lock()
            .values()
            .filter(|(owner, _, _)| owner == seat)
            .map(|(_, draft, _)| Ask {
                id: draft.id,
                kind: PendingInteractionKind::SlackDraft,
                payload: PendingAsk::SlackDraft(Box::new(draft.clone())),
            })
            .collect()
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        let count = self.0.lock().values().filter(|(owner, _, _)| owner == seat).count();
        vec![(PendingInteractionKind::SlackDraft, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::SlackDraft];
        KINDS
    }
}

/// Browser hand-offs parked for the person at a client.
pub(crate) struct BrowserHandOffs(pub Arc<Mutex<HashMap<Uuid, ParkedBrowserHandOff>>>);

impl AskSource for BrowserHandOffs {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.0
            .lock()
            .values()
            .filter(|(owner, _, _)| owner == seat)
            .map(|(_, handoff, _)| Ask {
                id: handoff.id,
                kind: PendingInteractionKind::BrowserHandOff,
                payload: PendingAsk::BrowserHandOff(Box::new(handoff.clone())),
            })
            .collect()
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        let count = self.0.lock().values().filter(|(owner, _, _)| owner == seat).count();
        vec![(PendingInteractionKind::BrowserHandOff, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::BrowserHandOff];
        KINDS
    }
}

/// Terminal hand-offs parked for the person at a client.
pub(crate) struct TerminalHandOffs(pub Arc<Mutex<HashMap<Uuid, ParkedTerminalHandOff>>>);

impl AskSource for TerminalHandOffs {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.0
            .lock()
            .values()
            .filter(|(owner, _, _)| owner == seat)
            .map(|(_, handoff, _)| Ask {
                id: handoff.id,
                kind: PendingInteractionKind::TerminalHandOff,
                payload: PendingAsk::TerminalHandOff(Box::new(handoff.clone())),
            })
            .collect()
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        let count = self.0.lock().values().filter(|(owner, _, _)| owner == seat).count();
        vec![(PendingInteractionKind::TerminalHandOff, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::TerminalHandOff];
        KINDS
    }
}

/// Document hand-offs parked for the person at a client.
pub(crate) struct DocumentHandOffs(pub Arc<Mutex<HashMap<Uuid, ParkedDocument>>>);

impl AskSource for DocumentHandOffs {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.0
            .lock()
            .values()
            .filter(|held| &held.seat == seat)
            .map(|held| Ask {
                id: held.ask.id,
                kind: PendingInteractionKind::DocumentHandOff,
                payload: PendingAsk::DocumentHandOff(Box::new(held.ask.clone())),
            })
            .collect()
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        let count = self.0.lock().values().filter(|held| &held.seat == seat).count();
        vec![(PendingInteractionKind::DocumentHandOff, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::DocumentHandOff];
        KINDS
    }
}

/// Configuration writes held for the reader's approval.
pub(crate) struct SettingsWrites(pub Arc<Mutex<HashMap<Uuid, ParkedSettingsWrite>>>);

impl AskSource for SettingsWrites {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.0
            .lock()
            .values()
            .filter(|(owner, _, _)| owner == seat)
            .map(|(_, ask, _)| Ask {
                id: ask.id,
                kind: PendingInteractionKind::SettingsWrite,
                payload: PendingAsk::SettingsWrite(Box::new(ask.clone())),
            })
            .collect()
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        let count = self.0.lock().values().filter(|(owner, _, _)| owner == seat).count();
        vec![(PendingInteractionKind::SettingsWrite, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::SettingsWrite];
        KINDS
    }
}
