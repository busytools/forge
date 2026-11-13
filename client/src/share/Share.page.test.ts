// @vitest-environment jsdom
/**
 * The share sheet's box: what a landed take does with its words.
 *
 * **The seat's read is mocked**, as the review and files pages mock it: what
 * this file is about is what the sheet does with a carried notice, not the
 * subscription that carries the record (`session/live.test.ts` covers that
 * over a real socket).
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const seat = vi.hoisted(() => ({ listeners: new Set<(read: unknown) => void>() }));

vi.mock('../session/live', () => ({
  watchSession: () => ({
    subscribe: (fn: (read: unknown) => void) => {
      seat.listeners.add(fn);
      return () => {
        seat.listeners.delete(fn);
      };
    },
  }),
}));

/** A microphone that opens without an audio stack; the take's wire is not
 *  the subject here. */
const mic = vi.hoisted(() => ({
  open: (): Promise<unknown> =>
    Promise.resolve({
      onFrame: null as ((bytes: Uint8Array) => void) | null,
      resolved: undefined,
      flush: () => null,
      flushWorklet: () => undefined,
      stop: () => undefined,
    }),
}));

vi.mock('../composer/mic', () => ({
  Microphone: { open: mic.open },
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
  micSilence: (): Promise<null> => Promise.resolve(null),
  silenceCause: (): null => null,
}));

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Share from './Share.svelte';

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** The key the landing memory stores one seat's last take under. */
const TAKE_KEY = `forge.dictate.take.${[SLOT.org, SLOT.project, SLOT.label].join('\u0000')}`;

/** Hand the page the record it would have folded out of a frame. */
function push(read: unknown): void {
  for (const fn of [...seat.listeners]) fn(read);
}

/** One seat's record with a landed notice, as the sheet reads it. */
function record(notice: Record<string, unknown> | null): unknown {
  return { wire: { composer: { take: null, notice, machine: null } }, refused: null };
}

let app: Record<string, unknown> | null = null;

/** A socket that records the commands the sheet sends. */
function fakeConnection(): { sent: Record<string, unknown>[]; connection: Connection } {
  const sent: Record<string, unknown>[] = [];
  return {
    sent,
    connection: {
      settings: () => null,
      dispatch(command: unknown): null {
        sent.push(command as Record<string, unknown>);
        return null;
      },
      status: () => 'open',
      onStatus: () => () => {},
      onMessage: () => () => {},
      frame: () => true,
    } as unknown as Connection,
  };
}

function mountSheet(connection: Connection, dictate = false): void {
  app = mount(Share, {
    target: document.body,
    props: {
      connection,
      seats: [SLOT],
      shared: {
        files: [{ path: '/cache/share/1/photo.jpg', name: 'photo.jpg', size: 10, mime: null }],
        error: null,
        empty: false,
      },
      dictate,
      onclose: () => {},
    },
  });
  flushSync();
}

function box(): HTMLTextAreaElement {
  const field = document.querySelector('textarea');
  if (field === null) throw new Error('the sheet drew no box');
  return field;
}

beforeEach(() => {
  (globalThis as { matchMedia?: unknown }).matchMedia = () => ({
    matches: false,
    addEventListener: () => {},
    removeEventListener: () => {},
  });
  localStorage.clear();
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  seat.listeners.clear();
  localStorage.clear();
});

describe('the share sheet box', () => {
  /**
   * **The seat's record reaches the box's notice line**, which is what makes
   * the drop below meaningful: a pushed notice is read by the page.
   */
  it('draws a notice the record hands the box', () => {
    mountSheet(fakeConnection().connection);
    push(record({ kind: 'line', tone: 'q', text: 'the take failed' }));
    flushSync();
    expect(document.body.textContent).toContain('the take failed');
  });

  /**
   * **A carried notice for a share this sheet no longer holds is dropped.**
   * The sheet is keyed on the share, so a second share is a fresh box; the
   * record's re-hand of the last share's words must not be guessed into it,
   * even when the stored generation says this client dictated them.
   */
  it('drops a carried notice rather than reusing the last share box', () => {
    localStorage.setItem(TAKE_KEY, '7');
    mountSheet(fakeConnection().connection);
    push(record({ kind: 'landed', text: 'hello', generation: 7, carried: true }));
    flushSync();
    expect(box().value, "the previous share's words landed in this one").toBe('');
  });
});

describe('the share seat picker', () => {
  it('draws the page-drawn picker rather than a native select', () => {
    mountSheet(fakeConnection().connection);
    expect(document.querySelector('select'), 'no native select is left').toBeNull();

    const trigger = document.querySelector<HTMLButtonElement>('.pick');
    expect(trigger, 'the seat row is the shared picker').not.toBeNull();
    expect(trigger?.textContent).toContain('forge');
  });

  it('opens the sheet on the picked seat, and a pick moves the seat', () => {
    const seatB: SessionSlot = { org: 'Busytools', project: 'busymail', label: 'growth' };
    app = mount(Share, {
      target: document.body,
      props: {
        connection: fakeConnection().connection,
        seats: [SLOT, seatB],
        shared: {
          files: [{ path: '/cache/share/1/photo.jpg', name: 'photo.jpg', size: 10, mime: null }],
          error: null,
          empty: false,
        },
        dictate: false,
        onclose: () => {},
      },
    });
    flushSync();

    const trigger = document.querySelector<HTMLButtonElement>('.pick');
    trigger?.click();
    flushSync();
    expect(document.querySelector('.sheet'), 'the picker opened its sheet').not.toBeNull();
    expect(document.querySelector('.pickopt[aria-current="true"]')?.textContent).toContain('forge');

    const rows = [...document.querySelectorAll<HTMLElement>('.pickopt')];
    rows[1]?.click();
    flushSync();
    expect(document.querySelector<HTMLButtonElement>('.pick')?.textContent).toContain('busymail');
  });
});

describe('the share take', () => {
  /** A press and its release, which is what a click on the mic delivers. */
  function pressMic(button: HTMLButtonElement): void {
    button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    button.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
    button.click();
  }

  /**
   * **The card is not the whole row.** A take started on the share page is
   * finished by the same mic that started it, drawn beside the unchanged
   * card - the composer's rule, and the defect issue #2968 names.
   */
  it('keeps the standard mic beside the unchanged card, and its press finishes', async () => {
    const fake = fakeConnection();
    mountSheet(fake.connection, true);

    const start = document.querySelector<HTMLButtonElement>('.acts .mic');
    if (start === null) throw new Error('the idle box drew no mic');
    pressMic(start);
    await vi.waitUntil(() => document.querySelector('.tc') !== null, { timeout: 2_000 });
    flushSync();

    expect(document.querySelector('.tc'), 'the take card is drawn, as it was').not.toBeNull();
    expect(
      document.querySelector('.tc .x'),
      "the card's own abandon control is unchanged",
    ).not.toBeNull();

    const finish = document.querySelector<HTMLButtonElement>('.acts .mic');
    expect(finish, 'the mic stays beside the card').not.toBeNull();
    expect(finish?.getAttribute('aria-label')).toBe('stop dictating');
    expect(
      document.querySelector('.acts')?.classList.contains('on'),
      'the live row is marked, which is what turns the mic orange',
    ).toBe(true);
    expect(
      document.querySelector<HTMLButtonElement>('.pick')?.disabled,
      'the seat cannot change under a running take',
    ).toBe(true);
    expect(
      document.querySelector<HTMLButtonElement>('.acts .chip.add')?.disabled,
      'and the send chip is frozen while the take runs',
    ).toBe(true);

    if (finish === null) throw new Error('no mic to finish with');
    pressMic(finish);
    await vi.waitUntil(() => fake.sent.some((command) => 'dictate_stop' in command), {
      timeout: 2_000,
    });
    const stop = fake.sent.find((command) => 'dictate_stop' in command);
    expect((stop?.dictate_stop as { submit?: boolean } | undefined)?.submit).toBe(true);
  });
});
