// @vitest-environment jsdom
/**
 * The Files page's own glyph wiring: the root row wears the theme's
 * root-folder glyph (its path is the lookup key when the manifest names a
 * root, and 5.39.0's root-name map is empty), a shut folder takes its shut
 * glyph and an open one its open glyph, and a file row takes its own. **The
 * seat's read is mocked**, as the review page's test mocks it: what this
 * file is about is what the page hands the tree, not the subscription that
 * carries the record, which `session/live.test.ts` covers over a real socket.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

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

/** A microphone that opens without an audio stack; the take's wire is the
 *  subject here, not the capture. */
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
import Files from './Files.svelte';

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** Hand the page the record it would have folded out of a frame. */
function push(read: unknown): void {
  for (const fn of [...seat.listeners]) fn(read);
}

/** The seat's record, as the page reads it - only those fields are real. */
function record(scanCwd: string): unknown {
  return {
    state: { scan_cwd: scanCwd },
    file_index: {
      entries: {
        'src/main.rs': { rel_path: 'src/main.rs', depth: 1 },
        'crates/forge/lib.rs': { rel_path: 'crates/forge/lib.rs', depth: 2 },
        README: { rel_path: 'README', depth: 0 },
      },
    },
    tree_reviews: {
      threads: { outcome: 'read', value: [] },
      reviews: { outcome: 'read', value: [] },
    },
  };
}

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  seat.listeners.clear();
});

describe('the files page tree', () => {
  it('draws the root row from the scan path with the theme root folder', () => {
    app = mount(Files, {
      target: document.body,
      props: { connection: {} as Connection, slot: SLOT, dictation: false },
    });
    flushSync();
    push({ wire: record('/Users/me/proj'), refused: null });
    flushSync();

    const root = document.querySelector('.rv-root');
    expect(root, 'the root row drew').not.toBeNull();
    expect(root?.querySelector('.rv-rootname')?.textContent, 'named for the project').toBe('forge');
    expect(
      root?.querySelector('.rv-rootpath')?.getAttribute('title'),
      'the seat scanning path beside it',
    ).toBe('/Users/me/proj');
    expect(
      (root?.querySelector('img.rv-icon')?.getAttribute('src') ?? '').split('?')[0],
      'the theme root folder, keyed off the path',
    ).toMatch(/\/folder-root-open\.svg$/);

    const shut = document.querySelector('[data-path="src"] img.rv-icon');
    expect(
      (shut?.getAttribute('src') ?? '').split('?')[0],
      'a shut folder takes its shut glyph',
    ).toMatch(/\/folder-src\.svg$/);

    const open = document.querySelector('[data-path="crates"] img.rv-icon');
    expect(
      (open?.getAttribute('src') ?? '').split('?')[0],
      'a folder-only folder opens and takes its open glyph',
    ).toMatch(/\/folder-lib-open\.svg$/);

    const file = document.querySelector('[data-path="README"] img.rv-icon');
    expect(
      (file?.getAttribute('src') ?? '').split('?')[0],
      'and a file row takes its own glyph',
    ).toMatch(/\/readme\.svg$/);
  });
});

describe('the files page take', () => {
  /** A socket that records the commands the page sends. */
  function fakeConnection() {
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

  /**
   * **The take names this page**, so the record can hand its words back here
   * and nowhere else. A copied name still compiles, so the wire itself is
   * what this pins.
   */
  it('names this page on its take command', async () => {
    // jsdom has no media queries, and the page's rail reads one to decide
    // whether its summary box is up.
    (globalThis as { matchMedia?: unknown }).matchMedia = () => ({
      matches: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    });
    const fake = fakeConnection();
    app = mount(Files, {
      target: document.body,
      props: { connection: fake.connection, slot: SLOT, dictation: true },
    });
    flushSync();
    push({ wire: record('/Users/me/proj'), refused: null });
    flushSync();

    const button =
      document.querySelector<HTMLButtonElement>('.files-railhold .mic') ??
      document.querySelector<HTMLButtonElement>('.mic');
    if (button === null) throw new Error('the files page drew no mic');
    button.click();
    await vi.waitFor(
      () => {
        expect(
          fake.sent.some((command) => command['dictate_stream'] !== undefined),
          'the mic began no take',
        ).toBe(true);
      },
      { timeout: 2000 },
    );
    const start = fake.sent.find((command) => command['dictate_stream'] !== undefined);
    expect(start?.['dictate_stream'], 'the take names this page').toMatchObject({
      surface: 'files',
    });
  });

  /**
   * **A carried notice for a box this page no longer holds is dropped**, even
   * with the rail's summary still stamped as the box the last take began in -
   * the re-hand names no box, so it is never written into the last one.
   */
  it('drops a carried notice rather than reusing its stale box', async () => {
    (globalThis as { matchMedia?: unknown }).matchMedia = () => ({
      matches: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    });
    const fake = fakeConnection();
    app = mount(Files, {
      target: document.body,
      props: { connection: fake.connection, slot: SLOT, dictation: true },
    });
    flushSync();
    const base = record('/Users/me/proj') as Record<string, unknown>;
    const at = (composer: unknown): unknown => ({ ...base, composer });
    push({
      wire: at({ take: null, notice: null, machine: null }),
      refused: null,
    });
    flushSync();

    const button = document.querySelector<HTMLButtonElement>('.files-finish .mic');
    if (button === null) throw new Error('the rail drew no mic');
    button.click();
    await vi.waitFor(
      () => {
        expect(
          fake.sent.some((command) => command['dictate_stream'] !== undefined),
          'the mic began no take',
        ).toBe(true);
      },
      { timeout: 2000 },
    );
    // The server shows the take, lands its words, and the page places them;
    // then the record re-hands the seat's words carried.
    push({
      wire: at({
        take: {
          phase: 'recording',
          levels: [],
          peak_db: -50,
          progress: { done: 0, total: null },
          elapsed_ms: 1200,
          floor_db: -50,
        },
        notice: null,
        machine: null,
      }),
      refused: null,
    });
    flushSync();
    push({
      wire: at({
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false, generation: 1 },
        machine: null,
      }),
      refused: null,
    });
    flushSync();
    // The reader's release: the take ends and the field comes back with the
    // words the live landing placed.
    button.click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.files-finish .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );
    expect(
      document.querySelector<HTMLTextAreaElement>('.files-finish .rv-txarea')?.value ?? '',
      'the live landing reaches the rail box',
    ).toContain('the words');

    push({
      wire: at({
        take: null,
        notice: {
          kind: 'landed',
          text: 'the unsent words',
          truncated: false,
          carried: true,
          generation: 4,
        },
        machine: null,
      }),
      refused: null,
    });
    flushSync();
    expect(
      document.querySelector<HTMLTextAreaElement>('.files-finish .rv-txarea')?.value ?? '',
      'the re-hand names no box, so it is dropped rather than written into the last one',
    ).not.toContain('the unsent words');
  });
});
