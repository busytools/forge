// @vitest-environment jsdom
/**
 * The dictation panel through a real page's turns.
 *
 * **The panel is a popover with state, and a turn is the thing that keeps
 * arriving under it.** #2524 was filed on it flickering while a turn runs, so
 * what this pins is the part the page owns: the seat's frames land as new
 * records, and the panel is still the same element with the same settings the
 * reader chose - not closed, not remounted, its state not reset.
 *
 * **A guard, not the regression's evidence.** This is green on main: frames
 * never did remount the panel. What #2524 actually measured - the panel riding
 * the turn strip, its models read re-subscribed on every page update, and the
 * row it hangs off going away - is pinned by the `Composer.test.ts` panel
 * tests, which fail without the fix. This sits above them so a page that later
 * remounts the composer or keys it on the record cannot pass unnoticed.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import { sessionFrom } from '../session/wire';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import Router from './Router.svelte';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/** The home wire with every dictation model ready, which is what offers the mic. */
function offered(): typeof homeWire {
  return {
    ...homeWire,
    dictate: {
      ...homeWire.dictate,
      snapshot: {
        ...homeWire.dictate.snapshot,
        models: homeWire.dictate.snapshot.models.map((model) => ({
          ...model,
          state: 'ready' as const,
        })),
      },
    },
  };
}

/**
 * A connection a seat's page can run over: the stores are real (a frame folds
 * the way it does on the socket), and the test speaks the seat's own messages
 * through `say`.
 */
function live(): { connection: Connection; say: (message: ServerMessage) => void; stores: Stores } {
  const stores = new Stores();
  const listeners = new Set<(message: ServerMessage) => void>();
  const quiet = (): (() => void) => () => {};
  const connection = {
    subscribe: (what: Subject) => stores.open(what),
    unsubscribe: (what: Subject) => stores.close(what),
    refresh: () => true,
    showing: () => {},
    relist: () => {},
    release: () => {},
    dispatch: () => null,
    more: () => false,
    seatRange: () => false,
    frame: () => false,
    onBrowserAsk: quiet,
    browserRole: () => true,
    browserRoleKnown: () => true,
    onBrowserRole: quiet,
    onProfilesChanged: quiet,
    takeBrowserRole: () => {},
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => {
        listeners.delete(fn);
      };
    },
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      fn('open');
      return () => {};
    },
    store: (what: Subject) => stores.get(what),
    settings: () => DEFAULT_SETTINGS,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open' as const,
    close: () => {},
  } as unknown as Connection;
  return {
    stores,
    connection,
    say: (message) => {
      for (const fn of listeners) fn(message);
    },
  };
}

/** A frame of the seat's stream, as the server numbers one. */
function frame(
  seq: number,
  at: SessionSlot,
): { kind: 'update'; update: SessionUpdate; seq: number } {
  // The head of a turn is the frame that says it is in flight, and the rest
  // are what streams under it.
  const msg =
    seq === 1
      ? { type: 'system', subtype: 'init', session_id: 's-1', model: 'claude-opus-5-5' }
      : {
          type: 'assistant',
          uuid: `u${String(seq)}`,
          message: { role: 'assistant', content: [{ type: 'text', text: `frame ${String(seq)}` }] },
        };
  return {
    kind: 'update',
    seq,
    update: { chat_appended: { key: at, msg } },
  };
}

/** Hold the mic, which is the gesture that opens the panel. */
function holdMic(): void {
  const mic = document.querySelector('.mic');
  if (!(mic instanceof HTMLElement)) throw new Error('the box drew no mic to open with');
  vi.useFakeTimers();
  try {
    mic.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    vi.advanceTimersByTime(450);
    mic.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
    mic.click();
  } finally {
    vi.useRealTimers();
  }
  flushSync();
}

/**
 * Land a message and let the seat's publish reach the page.
 *
 * The record is written on the paint the seat coalesces to, so the frame is
 * advanced under the fake clock the coalescer armed on - with real timers the
 * publish would outlive the assertion.
 */
function land(say: (message: ServerMessage) => void, message: ServerMessage): void {
  vi.useFakeTimers();
  try {
    say(message);
    vi.advanceTimersByTime(20);
  } finally {
    vi.useRealTimers();
  }
  flushSync();
}

describe("the dictation panel through a turn's frames", () => {
  it('keeps the panel, its element and its settings as the frames land', () => {
    const wire = live();
    app = mount(Router, {
      target: document.body,
      props: {
        route: { name: 'session', slot: LEAD },
        settings: DEFAULT_SETTINGS,
        address: '127.0.0.1:8790',
        home: { wire: offered(), refused: null, failed: null, report: null },
        failure: null,
        connected: true,
        connection: wire.connection,
        notice: null,
        onconnect: () => {},
      },
    });
    flushSync();

    const seeded = sessionFrom({ slot: LEAD, state: { scan_cwd: '/tmp' } });
    wire.stores.open({ session: LEAD }).set(seeded);
    wire.say({ kind: 'snapshot', subject: { session: LEAD }, data: seeded });
    flushSync();

    holdMic();
    const panel = document.querySelector('.pop');
    if (!(panel instanceof HTMLElement)) throw new Error('the hold drew no panel');
    // A setting this reader chose, which a remount would reset to the default.
    const formal = [...panel.querySelectorAll<HTMLButtonElement>('.chip')].find(
      (chip) => chip.textContent?.trim() === 'formal',
    );
    if (formal === undefined) throw new Error('the panel drew no voice chips');
    formal.click();
    flushSync();
    expect(formal.getAttribute('aria-pressed'), 'the chip did not take').toBe('true');

    for (let seq = 1; seq <= 6; seq += 1) {
      land(wire.say, frame(seq, LEAD));
    }

    // The frames landed - the turn they opened is what draws the stop.
    expect(
      document.querySelector('.stop'),
      'the frames never reached the page, so nothing below is evidence',
    ).not.toBeNull();

    const after = document.querySelector('.pop');
    expect(after, 'the panel closed on the turn').not.toBeNull();
    expect(after, 'the panel was remounted under the turn').toBe(panel);
    const still = [...panel.querySelectorAll<HTMLButtonElement>('.chip')].find(
      (chip) => chip.textContent?.trim() === 'formal',
    );
    expect(still?.getAttribute('aria-pressed'), "the turn reset the panel's own setting").toBe(
      'true',
    );
  });
});
