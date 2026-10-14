// @vitest-environment jsdom
/**
 * The review page's own take: the words a landed take leaves go into the box
 * that began it, and the bar takes that box's line while it runs.
 *
 * **The seat's read is mocked**, because what this file is about is the page's
 * half of the bargain - which box a landing belongs to - and not the
 * subscription that carries the record, which `session/live.test.ts` covers
 * over a real socket. The record the page folds is driven straight into it.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

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
}));

vi.mock('@git-diff-view/core', () => ({
  DiffFile: class {},
}));

vi.mock('@git-diff-view/svelte', async () => {
  const { default: Viewer } = await import('./testing/Viewer.svelte');
  return { DiffView: Viewer, DiffModeEnum: { Split: 3, Unified: 4 } };
});

/** The seat's read, as the page subscribes it: real enough to carry a record
 *  in, which is all this file needs of the one the socket serves. */
const seat = vi.hoisted(() => ({ listeners: new Set<(read: unknown) => void>() }));

vi.mock('../session/live', () => ({
  watchSession: () => ({
    subscribe: (fn: (read: unknown) => void) => {
      seat.listeners.add(fn);
      return () => {
        seat.listeners.delete(fn);
      };
    },
    hold: () => () => {},
  }),
}));

/** Hand the page the record it would have folded out of a frame. */
function push(read: unknown): void {
  for (const fn of [...seat.listeners]) fn(read);
}

import { installResizeObserver } from '../chat/testing/viewport';
import type { Connection } from '../socket';
import type { SessionRecord } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import Review from './Review.svelte';

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** One file with one line, which the comment box's door is drawn on. */
const DIFF = {
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

let app: Record<string, unknown> | null = null;
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

/** The work a mount leaves for later: the viewer's mocked import landing, and
 *  the reconciler's frames. */
async function settle(until: () => boolean): Promise<void> {
  for (let round = 0; round < 60; round += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
    flushSync();
    paint();
    if (until()) return;
  }
  throw new Error(
    `the page never drew what the test waited for: ${document.body.innerHTML.slice(0, 4000)}`,
  );
}

/** One thread, placed on the diff's single line. */
const THREAD = {
  id: 't1',
  anchor: {
    path: 'src/a.rs',
    side: 'new',
    line: 1,
    content_hash: '1',
    context: ['one'],
    base_ref: 'main',
  },
  comments: [{ author: { kind: 'user' }, text: 'the opening', at: 'x', review_id: null }],
  status: 'open',
  created_at: 'x',
  updated_at: 'x',
  commit: null,
  resolution: { state: 'placed', line: 1, moved_from: null },
};

/** The seat's record, as the page reads it - only the fields it reads are real. */
function record(composer: unknown, threads: unknown[] = []): SessionRecord {
  return {
    slot: SLOT,
    composer,
    work: { branch: 'b', dirt: 'clean' },
    git: { defaultBranch: 'main', ahead: null },
    diff: { branchAhead: DIFF },
    reviews: {
      threads: { outcome: 'read', value: threads },
      reviews: { outcome: 'read', value: [] },
    },
  } as unknown as SessionRecord;
}

/** The seat's composer, with nothing in flight - the state before a take. */
const IDLE = {
  take: null,
  notice: null,
  compacting: false,
  signIn: null,
  bind: 'right_cmd',
  mode: 'auto',
};

/** The take the record shows once the server has registered the page's own. */
const TAKE = {
  ...IDLE,
  take: {
    phase: 'recording',
    levels: [],
    peakDb: -50,
    progress: { done: 0, total: null },
    elapsedMs: 1200,
    floorDb: -50,
  },
};

/** The words the record holds once the take landed. */
const LANDED = {
  ...TAKE,
  take: null,
  notice: { kind: 'landed', text: 'this read is the whole input', truncated: false },
};

function connection(): Connection {
  return {
    settings: () => null,
    dispatch: () => null,
    refresh: () => undefined,
    commitDiff: () => undefined,
    status: () => 'open',
    onStatus: () => () => undefined,
    onMessage: () => () => undefined,
  } as unknown as Connection;
}

/** The page over a seat whose record the test drives, drawn to the point its
 *  diff's own doors are on screen. */
async function draw(threads: unknown[] = []): Promise<void> {
  app = mount(Review, {
    target: document.body,
    props: { connection: connection(), slot: SLOT, dictation: true },
  });
  flushSync();
  push({ wire: record(IDLE, threads), refused: null });
  // Unified, in which each row carries one number cell: the stood-in viewer
  // draws that shape, and the doors hang off the lines' own rows.
  await settle(() => {
    const button = [...document.querySelectorAll<HTMLButtonElement>('.rv-mode button')].find(
      (held) => held.textContent?.trim() === 'unified',
    );
    if (button === undefined) return false;
    if (!button.classList.contains('on')) {
      button.click();
      return false;
    }
    return document.querySelector('button.rv-add') !== null;
  });
}

/** Open the comment box on the diff's one line. */
async function openComment(): Promise<void> {
  const door = document.querySelector<HTMLButtonElement>('button.rv-add');
  if (door === null) throw new Error('the canvas drew no door to comment from');
  door.click();
  await settle(() => document.querySelector('.rv-thread.comment') !== null);
}

/** The conversation half, where the round's summary box lives. */
async function openConversation(): Promise<void> {
  const tabButton = [...document.querySelectorAll<HTMLButtonElement>('.rv-tabs > button')].find(
    (button) => button.textContent?.trim() === 'Conversation',
  );
  if (tabButton === undefined) throw new Error('the page drew no Conversation tab');
  tabButton.click();
  await settle(() => document.querySelector('.rv-finishrow') !== null);
}

/** The reply box under the diff's thread, opened by its own respond. */
async function openReply(): Promise<void> {
  await settle(() => document.querySelector('tr.rv-insert') !== null);
  const respond = [...document.querySelectorAll<HTMLButtonElement>('tr.rv-insert button')].find(
    (button) => button.textContent?.trim() === 'respond',
  );
  if (respond === undefined) throw new Error('the thread card drew no respond');
  respond.click();
  await settle(() => document.querySelector('.rv-reply') !== null);
}

function micIn(where: string): HTMLButtonElement {
  const found = document.querySelector<HTMLButtonElement>(`${where} .mic`);
  if (found === null) throw new Error(`${where} drew no mic`);
  return found;
}

/** The page's take, from a box's mic to its landing, and the words it left. */
async function dictateInto(where: string, threads: unknown[] = []): Promise<void> {
  micIn(where).click();
  await vi.waitFor(
    () => {
      expect(
        document.querySelector(`${where} .takeline`),
        "the bar takes the box's line",
      ).not.toBeNull();
    },
    { timeout: 2000 },
  );
  // The server registers the take where the record can see it, and then the
  // release lands the words - the order the frames really arrive in.
  push({ wire: record(TAKE, threads), refused: null });
  flushSync();
  push({ wire: record(LANDED, threads), refused: null });
  // The reader's release: the take ends, the bar goes with it, and the box
  // draws the words that landed in it. The release drains the capture on real
  // timers (#2159), so this waits in real time rather than in frames.
  micIn(where).click();
  await vi.waitFor(
    () => {
      expect(document.querySelector(`${where} .takeline`), 'the bar goes with the take').toBeNull();
    },
    { timeout: 2000 },
  );
}

describe('a landing', () => {
  it('goes into the comment box that began the take', async () => {
    await draw();
    await openComment();
    await dictateInto('.rv-thread.comment');

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      "the take's words, at the end of the box that began it",
    ).toBe('this read is the whole input');
  });

  it('goes into the reply box that began the take, and nowhere else', async () => {
    await draw([THREAD]);
    await openComment();
    await openReply();

    // The reply box's own mic: the comment box is open beside it, which is
    // what makes the routing visible - the words must go where the reader
    // pressed, not into the box that merely happens to be on screen.
    await dictateInto('.rv-reply', [THREAD]);

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-reply .rv-txarea')?.value,
      'the reply box, where the mic was pressed',
    ).toBe('this read is the whole input');
    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      'and nothing in the box that did not begin it',
    ).toBe('');
  });

  it('goes into the round summary that began the take', async () => {
    await draw();
    await openConversation();

    await dictateInto('.rv-finishrow');

    expect(
      document.querySelector<HTMLInputElement>('.rv-summary')?.value,
      'the round summary, where the mic was pressed',
    ).toBe('this read is the whole input');
  });
});

describe("the take's bar", () => {
  it("takes the line of the box that began the take, and no other box's", async () => {
    await draw([THREAD]);
    await openComment();
    await openReply();

    micIn('.rv-thread.comment').click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.rv-thread.comment .takeline'),
          'the bar took the line of the box whose mic was pressed',
        ).not.toBeNull();
      },
      { timeout: 2000 },
    );

    // The reply box is open beside it, so a bar drawn for every box on screen
    // would show here - the take is the comment box's, and the reply box keeps
    // drawing its field.
    expect(
      document.querySelector('.rv-reply .tc'),
      "and drew nothing in a box the take is not this one's",
    ).toBeNull();
    expect(
      document.querySelector('.rv-reply .rv-txarea'),
      'which still draws the field it was drawing',
    ).not.toBeNull();
  });
});
