// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * The rows the canvas builds into the viewer's table, over the page's clock.
 *
 * **The tick has to reach a mounted row.** A row the reconciler keeps carries
 * the props it was built with, so a snapshot clock froze its age chip at the
 * mount while the conversation half's kept moving; read through a getter it
 * re-stamps in place on the tick, with no frame in between.
 *
 * The viewer pair is stood in for (jsdom cannot mount the real one - its text
 * measurement allocates without bound there), and the harness drives its own
 * async: the mocked import's landing is waited for by condition, and the frame
 * the reconciler asks for runs when the test paints it.
 */

vi.mock('@git-diff-view/core', () => ({
  DiffFile: class {},
}));

vi.mock('@git-diff-view/svelte', async () => {
  const { default: Viewer } = await import('./testing/Viewer.svelte');
  return { DiffView: Viewer, DiffModeEnum: { Split: 3, Unified: 4 } };
});

import { installResizeObserver } from '../chat/testing/viewport';
import { Dictation, openCapture } from '../composer/dictate.svelte';
import { wire as fakeWire } from '../composer/testing';
import { DEFAULT_AXES, type DiffLayerRecord, type ReviewThreadRecord } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import Canvas from './Canvas.svelte';

const T0 = Date.parse('2026-10-13T12:00:00Z');
/** The stamp the chip reads as `29m`, a tick short of `30m`. */
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
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          lines: [{ kind: 'context', text: 'one', oldLine: 1, newLine: 1 }],
        },
      ],
    },
  ],
};

/** One answered thread on the first line, its answer carrying the age. */
const thread: ReviewThreadRecord = {
  id: 't1',
  anchor: {
    path: 'src/a.rs',
    side: 'new',
    line: 1,
    content_hash: '1',
    context: ['one'],
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
  resolution: { state: 'placed', line: 1, moved_from: null },
};

/** A thread the resolver placed at line 4, written at 7: the shape the
 *  server's Moved arm produces (moved_from is the recorded line). */
const moved: ReviewThreadRecord = {
  ...thread,
  anchor: { ...thread.anchor, line: 7 },
  resolution: { state: 'placed', line: 4, moved_from: 7 },
};

/** The same file with every line the moved card needs drawn. */
const taller: DiffLayerRecord = {
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
          oldCount: 5,
          newStart: 1,
          newCount: 5,
          lines: [
            { kind: 'context', text: 'one', oldLine: 1, newLine: 1 },
            { kind: 'context', text: 'two', oldLine: 2, newLine: 2 },
            { kind: 'context', text: 'three', oldLine: 3, newLine: 3 },
            { kind: 'context', text: 'four', oldLine: 4, newLine: 4 },
            { kind: 'context', text: 'five', oldLine: 5, newLine: 5 },
          ],
        },
      ],
    },
  ],
};

let app: Record<string, unknown> | null = null;
/** The frames the page asked for, run by [`paint`]. */
let frames: FrameRequestCallback[] = [];

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  frames = [];
  vi.restoreAllMocks();
  vi.useRealTimers();
});

beforeEach(() => {
  installResizeObserver();
  // jsdom carries no media queries a test can read, and the narrow regime
  // draws the sections plain, without the virtualised list.
  (globalThis as { matchMedia?: unknown }).matchMedia = () => ({
    matches: true,
    addEventListener: () => {},
    removeEventListener: () => {},
  });
  vi.spyOn(globalThis, 'requestAnimationFrame').mockImplementation((frame) => {
    frames.push(frame);
    return frames.length;
  });
});

/** Run the frames the page asked for, and the effects their mounts scheduled. */
function paint(): void {
  const asked = frames;
  frames = [];
  for (const frame of asked) frame(0);
  flushSync();
}

/**
 * The work a mount leaves for later: the viewer's mocked import landing, and
 * the reconciler's frame. The import resolves on a real macrotask, so the loop
 * yields to that queue and stops the moment the row is drawn - a busy machine
 * makes the rounds slower, never the outcome.
 */
async function settle(): Promise<void> {
  for (let round = 0; round < 50; round += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
    flushSync();
    paint();
    if (document.querySelector('tr.rv-insert') !== null) return;
  }
  throw new Error('the row was never drawn');
}

/** The canvas over the thread, as the page mounts it. */
function draw(held: ReviewThreadRecord = thread, at: DiffLayerRecord = layer): void {
  vi.useFakeTimers({ toFake: ['Date', 'setInterval', 'clearInterval'] });
  vi.setSystemTime(new Date(T0));
  const shared = fakeWire();
  const dictate = new Dictation({
    connection: () => shared.connection,
    seat: () => SEAT,
    shown: () => false,
    options: () => DEFAULT_AXES,
    device: () => null,
    capture: openCapture,
    onStart: () => undefined,
    onLine: () => undefined,
    onRefused: () => undefined,
    onEnded: () => undefined,
  });
  app = mount(Canvas, {
    target: document.body,
    props: {
      box: {
        spot: null,
        draft: '',
        busy: false,
        line: null,
        phase: null,
        panel: false,
        take: null,
        wire: null,
      },
      reply: {
        thread: null,
        index: null,
        draft: '',
        busy: false,
        line: null,
        phase: null,
        panel: false,
        take: null,
        wire: null,
      },
      voice: {
        dictate,
        offered: false,
        connection: null,
        bind: 'right_cmd',
        mode: 'auto',
        axes: DEFAULT_AXES,
        device: null,
        defaults: DEFAULT_AXES,
        onaxes: () => undefined,
        ondevice: () => undefined,
      },
      connection: null,
      slot: SEAT,
      scopeCommit: null,
      mode: 'unified',
      layer: at,
      threads: [held],
      reviews: [],
      busy: false,
      dictate,
      focusComment: null,
      onpress: () => undefined,
      onhold: () => undefined,
      onelement: () => undefined,
      oncomment: () => Promise.resolve(false),
      onreply: () => Promise.resolve(false),
      onresolve: () => undefined,
      onreopen: () => undefined,
    },
  });
}

/** The age chip the row's answered turn carries. */
function chip(): string | null {
  return document.querySelector('.rv-insert .rv-at')?.textContent ?? null;
}

/** Two files, so the tree can carry a reading mark on one of them. */
const twoFiles: DiffLayerRecord = {
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
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          lines: [{ kind: 'context', text: 'one', oldLine: 1, newLine: 1 }],
        },
      ],
    },
    {
      path: 'src/b.rs',
      oldPath: null,
      status: 'modified',
      binary: false,
      submodule: false,
      truncated: false,
      hunks: [
        {
          oldStart: 1,
          oldCount: 1,
          newStart: 1,
          newCount: 1,
          lines: [{ kind: 'context', text: 'one', oldLine: 1, newLine: 1 }],
        },
      ],
    },
  ],
};

/** The tree rows the reading mark is on, by basename. */
function marked(): string[] {
  return [...document.querySelectorAll('.rv-frow.here')].map(
    (row) => row.querySelector('.rv-fpath')?.textContent ?? '',
  );
}

describe('the file the reader is inside', () => {
  /**
   * **The mark follows the scroll.** The tree names the file whose region
   * holds the reading edge, and a mark that only moved on a click (or never
   * moved at all) would leave the reader's place in the list lying about
   * where they are.
   */
  it('marks the file the reading edge sits in, and moves the mark with the scroll', async () => {
    // The sections' own geometry, which jsdom does not lay out: file A's
    // region runs past the reading edge (the window's top at the narrow
    // width this harness mounts), file B's begins below it.
    let aTop = -200;
    let bTop = 100;
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
      this: HTMLElement,
    ) {
      const top = this.id === 'src/b.rs' ? bTop : aTop;
      return { top, bottom: top + 100, height: 100 } as DOMRect;
    });

    draw(thread, twoFiles);
    await settle();
    expect(marked(), 'the edge sits inside a').toEqual(['a.rs']);

    // The scroll: a's region is now above the edge and b's holds it.
    aTop = -300;
    bTop = -20;
    window.dispatchEvent(new Event('scroll'));
    paint();

    expect(marked(), 'the mark moved to the file the reader scrolled into').toEqual(['b.rs']);
    expect(
      document.querySelector('.rv-frow.here')?.getAttribute('aria-current'),
      'and it is a current-row mark, not only a class',
    ).toBe('true');
  });
});

describe('the clock a mounted row draws', () => {
  it('moves the age chip on the tick, as the conversation half does', async () => {
    draw();
    await settle();
    expect(chip(), 'the age the row was built with').toBe('29m');

    // The tick and one flush: the chip reads the clock the child subscribed
    // to as it drew, so nothing here needs a frame or a second pass.
    vi.advanceTimersByTime(30_000);
    flushSync();

    expect(chip(), 'the age the tick moved').toBe('30m');
  });

  /**
   * **A moved card names the line it sits on, and where the words were
   * written.** The card is inserted at the resolution's line, so `line N`
   * is that one - the recorded number reads as the move's origin, never
   * beside it as a second claim (the shape `line 7 · moved from 7` the
   * badge used to draw).
   */
  it('names the moved line the card sits on, and the line it was written at', async () => {
    draw(moved, taller);
    await settle();

    const where = document.querySelector('.rv-insert .rv-where')?.textContent ?? '';
    expect(where, 'the card names the row it sits on').toContain('line 4');
    const origin = document.querySelector('.rv-insert .rv-moved')?.textContent ?? '';
    expect(origin, 'and where it was written').toBe('moved from 7');
  });
});
