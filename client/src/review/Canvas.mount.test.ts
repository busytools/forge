// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * The rows the canvas builds into the viewer's table, over the page's clock.
 *
 * **The tick has to reach a mounted row.** The canvas builds its rows as DOM
 * and mounts a turn into each, so a row the reconciler keeps carries the
 * props it was built with - and the age chip reads the page's clock. A
 * snapshot clock freezes the chip at the moment its row was mounted, which is
 * the half the conversation does not freeze: there the same piece is drawn by
 * the template, whose props stay live.
 *
 * The viewer pair is stood in for - jsdom cannot mount the real one, its text
 * measurement allocates without bound there - and nothing here reads the
 * model, so the stub is only the shapes the reconciler reads back.
 */

vi.mock('@git-diff-view/core', () => ({
  DiffFile: class {},
}));

vi.mock('@git-diff-view/svelte', async () => {
  const { default: Viewer } = await import('./testing/Viewer.svelte');
  return { DiffView: Viewer, DiffModeEnum: { Split: 3, Unified: 4 } };
});

import { installResizeObserver } from '../chat/testing/viewport';
import { Dictation } from '../composer/dictate.svelte';
import { wire as fakeWire } from '../composer/testing';
import { DEFAULT_AXES, type DiffLayerRecord, type ReviewThreadRecord } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import Canvas from './Canvas.svelte';

const T0 = Date.parse('2026-10-13T12:00:00Z');
/** The second turn's stamp: the chip reads `29m` at the mount, and the tick
 *  crosses the minute to `30m`. */
const AT = new Date(T0 - (29 * 60 + 45) * 1000).toISOString();

const SEAT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** One file, with the thread's line among the rows the stub draws. */
const layer: DiffLayerRecord = {
  state: 'populated',
  files: [
    {
      path: 'src/a.rs',
      oldPath: null,
      status: 'modified',
      binary: false,
      submodule: false,
      truncated: false,
      hunks: [
        {
          oldStart: 1,
          oldCount: 2,
          newStart: 1,
          newCount: 2,
          lines: [
            { kind: 'context', text: 'one', oldLine: 1, newLine: 1 },
            { kind: 'context', text: 'two', oldLine: 2, newLine: 2 },
          ],
        },
      ],
    },
  ],
};

/** One answered thread on the second line, its answer carrying the age. */
const thread: ReviewThreadRecord = {
  id: 't1',
  anchor: {
    path: 'src/a.rs',
    side: 'new',
    line: 2,
    content_hash: '1',
    context: ['one', 'two'],
    base_ref: 'main',
  },
  comments: [
    { author: { kind: 'user' }, text: 'the opening', at: '2026-10-13T11:00:00Z', review_id: null },
    { author: { kind: 'agent', label: 'w1' }, text: 'the answer', at: AT, review_id: null },
  ],
  status: 'addressed',
  created_at: '2026-10-13T11:00:00Z',
  updated_at: '2026-10-13T11:30:00Z',
  commit: null,
  resolution: { state: 'placed', line: 2, moved_from: null },
};

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.useRealTimers();
});

beforeEach(() => {
  installResizeObserver();
  // The narrow regime, so the sections draw plain and no list is needed.
  matchMediaTo(true);
});

/** Answer every media query with `matches`, as a narrow window would. */
function matchMediaTo(matches: boolean): void {
  (globalThis as { matchMedia?: unknown }).matchMedia = () => ({
    matches,
    addEventListener: () => {},
    removeEventListener: () => {},
  });
}

/**
 * The work a mount leaves for later, run by hand: the viewer's import, the
 * flush that follows it, and the frames the reconciler draws in.
 *
 * **`setTimeout` stays real**, because the mocked viewer's import resolves on
 * one - a fully faked clock stalls it - while the frame callback rides the
 * faked interval jsdom builds its animation frames on, so the advance is what
 * paints.
 */
async function settle(): Promise<void> {
  for (let round = 0; round < 5; round += 1) {
    await new Promise((resolve) => setTimeout(resolve, 5));
    flushSync();
    vi.advanceTimersByTime(20);
    flushSync();
  }
}

/** The canvas over the thread, as the page mounts it. */
async function draw(): Promise<void> {
  vi.useFakeTimers({ toFake: ['Date', 'setInterval', 'clearInterval'] });
  vi.setSystemTime(new Date(T0));
  const shared = fakeWire();
  const dictate = new Dictation({
    connection: () => shared.connection,
    seat: () => SEAT,
    shown: () => false,
    options: () => DEFAULT_AXES,
    device: () => null,
    onStart: () => undefined,
    onLine: () => undefined,
    onRefused: () => undefined,
    onEnded: () => undefined,
  });
  app = mount(Canvas, {
    target: document.body,
    props: {
      connection: null,
      slot: SEAT,
      scopeCommit: null,
      mode: 'unified',
      layer,
      threads: [thread],
      reviews: [],
      busy: false,
      composer: {
        take: null,
        notice: null,
        compacting: false,
        signIn: null,
        bind: 'right_cmd',
        mode: 'auto',
      },
      dictation: false,
      dictate,
      oncomment: () => Promise.resolve(false),
      onreply: () => Promise.resolve(false),
      onresolve: () => undefined,
      onreopen: () => undefined,
    },
  });
  await settle();
}

/** The age chip the row's answered turn carries. */
function chip(): string | null {
  return document.querySelector('.rv-insert .rv-at')?.textContent ?? null;
}

describe('the clock a mounted row draws', () => {
  it('moves the age chip on the tick, as the conversation half does', async () => {
    await draw();
    expect(chip(), 'the age the row was built with').toBe('29m');

    vi.advanceTimersByTime(30_000);
    flushSync();

    expect(chip(), 'the age the tick moved').toBe('30m');
  });
});
