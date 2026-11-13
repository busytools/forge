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
