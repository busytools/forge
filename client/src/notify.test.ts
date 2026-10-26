// @vitest-environment jsdom
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { askState, askWords, notifiesFromThePage, rising, type AskState } from './notify';
import type { AgentRow } from './wire/home';
import type { SessionSlot } from './wire/types';

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const OTHER: SessionSlot = { org: 'Busytools', project: 'forge', label: 'worker' };

function row(slot: SessionSlot, over: Partial<AgentRow> = {}): AgentRow {
  return {
    slot,
    label: slot.label,
    lifecycle: 'Running',
    has_background_work: false,
    pending: null,
    pending_depth: 0,
    pending_seq: 0,
    last_activity: null,
    reason: null,
    failed_turn: null,
    died: false,
    tree: null,
    work: null,
    ...over,
  };
}

/** State after the launch read: seeded, with nothing announced. */
function launched(): AskState {
  const state = askState();
  rising(state, 'run-1', [], null, true);
  return state;
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('which asks are news', () => {
  /// **A first read is the launch's own history.** A reload with three parked
  /// asks announces none of them.
  it('seeds the first read without announcing it', () => {
    const marks = askState();
    const parked = [
      row(LEAD, { pending: 'question', pending_depth: 1, pending_seq: 1 }),
      row(OTHER, { pending: 'permission', pending_depth: 1, pending_seq: 1 }),
    ];
    expect(rising(marks, 'run-1', parked, null, true), 'launch is not news').toEqual([]);
    expect(rising(marks, 'run-1', parked, null, true), 'and the second read agrees').toEqual([]);
  });

  /// **An ask on another seat is news.**
  it('announces an ask on a seat the person is not looking at', () => {
    const notices = rising(
      launched(),
      'run-1',
      [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 1 })],
      LEAD,
      true,
    );
    expect(notices).toEqual([{ slot: OTHER, kind: 'question' }]);
  });

  /// **The seat on screen is the person's own answer while they look at it**,
  /// and the same ask is announced once they look away.
  it('holds the seat on screen until it is no longer looked at', () => {
    const marks = launched();
    const rows = [row(LEAD, { pending: 'permission', pending_depth: 1, pending_seq: 1 })];
    expect(rising(marks, 'run-1', rows, LEAD, true), 'looked at, not announced').toEqual([]);
    expect(
      rising(marks, 'run-1', rows, LEAD, false),
      'the window lost focus, so it is news',
    ).toEqual([{ slot: LEAD, kind: 'permission' }]);
  });

  /// **A re-read is not a new ask.** The identity is the ask's sequence.
  it('does not announce the same ask twice', () => {
    const marks = launched();
    const rows = [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 4 })];
    expect(rising(marks, 'run-1', rows, LEAD, true)).toHaveLength(1);
    expect(rising(marks, 'run-1', rows, LEAD, true), 'the second read is the same ask').toEqual([]);
  });

  /// **Answering one of three is not an ask.** The sequence stays where it
  /// was, so a depth that shrinks is the batch, not news.
  it('does not announce a batch an answer shrank', () => {
    const marks = launched();
    const three = [row(OTHER, { pending: 'permission', pending_depth: 3, pending_seq: 5 })];
    expect(rising(marks, 'run-1', three, LEAD, true)).toHaveLength(1);
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'permission', pending_depth: 2, pending_seq: 5 })],
        LEAD,
        true,
      ),
      'one answered, none arrived',
    ).toEqual([]);
  });

  /// **An ask replacing another at the same depth is news**, which a
  /// kind-and-depth identity could not see.
  it('announces an ask replaced at the same depth', () => {
    const marks = launched();
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 5 })],
        LEAD,
        true,
      ),
    ).toHaveLength(1);
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'permission', pending_depth: 1, pending_seq: 6 })],
        LEAD,
        true,
      ),
      'a different ask took its place',
    ).toEqual([{ slot: OTHER, kind: 'permission' }]);
  });

  /// **A forge restart starts the counter again, so the run scopes the mark**:
  /// a mark from the dead run cannot swallow this run's first ask.
  it('scopes a mark to the server run', () => {
    const marks = launched();
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 1 })],
        LEAD,
        true,
      ),
    ).toHaveLength(1);
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 1 })],
        LEAD,
        true,
      ),
      'the same run and number is the same ask',
    ).toEqual([]);
    expect(
      rising(
        marks,
        'run-2',
        [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 1 })],
        LEAD,
        true,
      ),
      'the next run numbered 1 again is a different ask',
    ).toHaveLength(1);
  });

  /// **Answered asks clear the mark.** Otherwise the next ask on the same seat
  /// would be swallowed by the one before it.
  it('announces again after the ask is answered and a new one arrives', () => {
    const marks = launched();
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 1 })],
        LEAD,
        true,
      ),
    ).toHaveLength(1);
    expect(rising(marks, 'run-1', [row(OTHER)], LEAD, true)).toEqual([]);
    expect(
      rising(
        marks,
        'run-1',
        [row(OTHER, { pending: 'question', pending_depth: 1, pending_seq: 2 })],
        LEAD,
        true,
      ),
      'a new question is news',
    ).toHaveLength(1);
  });
});

describe('delivering an ask', () => {
  /// **The permission is asked from a user gesture**, because a browser that
  /// wants activation (Safari, Firefox) refuses the request made from an ask
  /// alone and nothing would ever show.
  it('asks for permission on the first gesture, not at the ask', async () => {
    vi.resetModules();
    const requestPermission = vi.fn(() => Promise.resolve('granted' as const));
    const shown = vi.fn();
    class Note {
      static permission: NotificationPermission = 'default';
      static requestPermission = requestPermission;
      onclick: (() => void) | null = null;
      constructor() {
        shown();
      }
      close(): void {}
    }
    vi.stubGlobal('Notification', Note);
    const fresh = await import('./notify');
    fresh.deliverAsk({ slot: OTHER, kind: 'question' });
    expect(requestPermission, 'an ask alone cannot prompt').not.toHaveBeenCalled();
    expect(shown, 'and nothing is shown before permission').not.toHaveBeenCalled();

    window.dispatchEvent(new Event('pointerdown'));
    await vi.waitFor(() => expect(shown).toHaveBeenCalledTimes(1));
    expect(requestPermission).toHaveBeenCalledTimes(1);
  });

  /// **A denied permission is respected silently**: no prompt, no notification.
  it('shows nothing when the permission was refused', async () => {
    vi.resetModules();
    const requestPermission = vi.fn(() => Promise.resolve('denied' as const));
    const shown = vi.fn();
    class Note {
      static permission: NotificationPermission = 'denied';
      static requestPermission = requestPermission;
      constructor() {
        shown();
      }
      close(): void {}
    }
    vi.stubGlobal('Notification', Note);
    const fresh = await import('./notify');
    fresh.deliverAsk({ slot: OTHER, kind: 'permission' });
    window.dispatchEvent(new Event('pointerdown'));
    expect(shown, 'a refusal shows nothing').not.toHaveBeenCalled();
    expect(requestPermission, 'and never nags').not.toHaveBeenCalled();
  });

  /// **A request that was no answer latches nothing, and keeps the ask**: the
  /// next gesture may ask again and still has something to show. A queue
  /// emptied before the answer arrives loses the ask a denial never refused.
  it('keeps a held ask for the next gesture that can ask', async () => {
    vi.resetModules();
    const requestPermission = vi
      .fn()
      .mockResolvedValueOnce('default' as const)
      .mockResolvedValueOnce('granted' as const);
    const shown = vi.fn();
    class Note {
      static permission: NotificationPermission = 'default';
      static requestPermission = requestPermission;
      onclick: (() => void) | null = null;
      constructor() {
        shown();
      }
      close(): void {}
    }
    vi.stubGlobal('Notification', Note);
    const fresh = await import('./notify');
    fresh.deliverAsk({ slot: OTHER, kind: 'question' });

    window.dispatchEvent(new Event('pointerdown'));
    await vi.waitFor(() => expect(requestPermission).toHaveBeenCalledTimes(1));
    expect(shown, 'no answer shows nothing').not.toHaveBeenCalled();

    window.dispatchEvent(new Event('keydown'));
    await vi.waitFor(() => expect(shown, 'the held ask is still there').toHaveBeenCalledTimes(1));
    expect(requestPermission, 'the next gesture asks again').toHaveBeenCalledTimes(2);
  });

  /// **A refusal latches for the asks already held.** Browsers never prompt
  /// again after a block, so the held queue is dropped and no later gesture
  /// asks - the alternative is an unbounded no-op loop on every gesture.
  it('drops the held asks and stops asking after a refusal', async () => {
    vi.resetModules();
    const requestPermission = vi.fn(() => Promise.resolve('denied' as const));
    const shown = vi.fn();
    class Note {
      static permission: NotificationPermission = 'default';
      static requestPermission = requestPermission;
      onclick: (() => void) | null = null;
      constructor() {
        shown();
      }
      close(): void {}
    }
    vi.stubGlobal('Notification', Note);
    const fresh = await import('./notify');
    fresh.deliverAsk({ slot: OTHER, kind: 'question' });
    fresh.deliverAsk({ slot: LEAD, kind: 'permission' });

    window.dispatchEvent(new Event('pointerdown'));
    await vi.waitFor(() => expect(requestPermission).toHaveBeenCalledTimes(1));
    expect(shown, 'a refusal shows nothing').not.toHaveBeenCalled();

    window.dispatchEvent(new Event('keydown'));
    window.dispatchEvent(new Event('pointerdown'));
    await Promise.resolve();
    expect(requestPermission, 'a refusal is never asked again').toHaveBeenCalledTimes(1);
    expect(shown, 'and the asks held at refusal are dropped').not.toHaveBeenCalled();
  });

  /// **The words name the seat and the ask**, and a lead's title is the
  /// project alone - the same seat a page title spells.
  it('names the seat and the ask in the notification', () => {
    expect(askWords({ slot: LEAD, kind: 'question' })).toEqual({
      title: 'forge \u{b7} forge',
      body: 'a question is waiting',
    });
    expect(askWords({ slot: OTHER, kind: 'hand_off' })).toEqual({
      title: 'forge \u{b7} forge \u{b7} worker',
      body: 'a hand-off is waiting',
    });
  });

  /// **A click opens the seat**, which is the only reason the notification is
  /// worth raising on a page.
  it('opens the seat when the notification is clicked', async () => {
    vi.resetModules();
    const shown: { onclick: (() => void) | null }[] = [];
    class Note {
      static permission: NotificationPermission = 'granted';
      static requestPermission = vi.fn(() => Promise.resolve('granted' as const));
      onclick: (() => void) | null = null;
      constructor(
        public title: string,
        public options: { body: string; tag: string },
      ) {
        shown.push(this);
      }
      close(): void {}
    }
    vi.stubGlobal('Notification', Note);
    // jsdom leaves `focus` unimplemented and logs on every call.
    vi.spyOn(window, 'focus').mockImplementation(() => undefined);
    const fresh = await import('./notify');
    // The reset registry's own route store is the one this module writes.
    const routes = await import('./routes');
    fresh.deliverAsk({ slot: OTHER, kind: 'question' });
    const [note] = shown;
    if (note === undefined) throw new Error('no notification was shown');
    note.onclick?.();
    expect(get(routes.currentRoute)).toEqual({ name: 'session', slot: OTHER });
  });

  /// **A shell's page raises none**: its Rust half does, so the page does not
  /// run the watcher that could not run while it is parked.
  it('leaves the raising to the shell where there is one', () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    try {
      expect(notifiesFromThePage()).toBe(false);
    } finally {
      Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
    }
    expect(notifiesFromThePage(), 'a page with no shell raises its own').toBe(true);
  });
});
