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
import { TAP_MS } from '../composer/dictate-key';
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
function record(
  composer: unknown,
  threads: unknown[] = [],
  reviews: unknown[] = [],
): SessionRecord {
  return {
    slot: SLOT,
    composer,
    work: { branch: 'b', dirt: 'clean' },
    git: { defaultBranch: 'main', ahead: null },
    diff: { branchAhead: DIFF },
    reviews: {
      threads: { outcome: 'read', value: threads },
      reviews: { outcome: 'read', value: reviews },
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

/** One filed round, so the conversation half draws its thread and its reply. */
const ROUND = { id: 'r1', number: 1, summary: null, created_at: 'x' };

/** A second thread on the same line, for the box-moves-mid-take case. */
const SECOND = { ...THREAD, id: 't2' };

/** The record while the server transcribes a take the page let go. */
const TRANSCRIBING = { ...TAKE, take: { ...TAKE.take, phase: 'transcribing' } };

/** The thread as a round carries it: its opening turn belongs to ROUND. */
const HELD_THREAD = {
  ...THREAD,
  comments: [{ ...THREAD.comments[0], review_id: 'r1' }],
};

/** The commands the page dispatched, cleared at each draw, so a test can
 *  read what a take's start or its end put on the wire. */
let sent: unknown[] = [];

function connection(): Connection {
  return {
    settings: () => null,
    dispatch: (command?: unknown) => {
      sent.push(command);
      return null;
    },
    refresh: () => undefined,
    commitDiff: () => undefined,
    status: () => 'open',
    onStatus: () => () => undefined,
    onMessage: () => () => undefined,
  } as unknown as Connection;
}

/** The page over a seat whose record the test drives, drawn to the point its
 *  diff's own doors are on screen. */
async function draw(
  threads: unknown[] = [],
  reviews: unknown[] = [],
  dictation = true,
): Promise<void> {
  sent = [];
  app = mount(Review, {
    target: document.body,
    props: { connection: connection(), slot: SLOT, dictation },
  });
  flushSync();
  push({ wire: record(IDLE, threads, reviews), refused: null });
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

/**
 * Let a take's start settle: opening the microphone is a promise, and the
 * command goes once it resolves.
 */
async function opened(): Promise<void> {
  for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
  flushSync();
}

/** The commands dispatched so far, as one string a failure message can carry. */
function wire(): string {
  return sent.map((command) => JSON.stringify(command)).join(' ');
}

/** The page's take, from a box's mic to its landing, and the words it left. */
async function dictateInto(
  where: string,
  threads: unknown[] = [],
  reviews: unknown[] = [],
): Promise<void> {
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
  push({ wire: record(TAKE, threads, reviews), refused: null });
  flushSync();
  push({ wire: record(LANDED, threads, reviews), refused: null });
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

  /**
   * **A quick take the record never showed still lands.** The record a page
   * reads is written once per painted frame, so a press and a release can fold
   * into one write with no take in it at all: the arm is the press's own, and
   * the words land in the box that began the take all the same (the composer's
   * own quick-release case, Ved, 2026-10-15).
   */
  it('lands a take the record never showed, after a quick release', async () => {
    await draw();
    await openComment();

    micIn('.rv-thread.comment').click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.rv-thread.comment .takeline'),
          "the bar takes the box's line",
        ).not.toBeNull();
      },
      { timeout: 2000 },
    );

    // The words land with the record never having shown a take: only the
    // press knows this take exists on the page's side.
    push({ wire: record(LANDED), refused: null });
    flushSync();

    // The release, and the box draws the words.
    micIn('.rv-thread.comment').click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.rv-thread.comment .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      'the words of a take the record never showed',
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

describe("a take's end", () => {
  /**
   * **A take goes with its box.** Closing the box mid-take ends the take:
   * without it the microphone keeps capturing into a box nothing draws, and
   * the words land where the reader cannot see them.
   */
  it('ends the take when its box closes', async () => {
    await draw();
    await openComment();

    micIn('.rv-thread.comment').click();
    await opened();
    push({ wire: record(TAKE), refused: null });
    flushSync();
    sent = [];

    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.rv-thread.comment .rv-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error('the comment box drew no cancel');
    cancel.click();
    flushSync();

    expect(sent, 'a take outlived the box that began it').toEqual([
      { dictate_stop: { key: SLOT, submit: false } },
    ]);
  });

  /**
   * **The summary's box is the conversation half's, so the files tab ends a
   * summary take too.** The box goes with the tab, and a take that kept
   * capturing would draw its bar nowhere and land its words out of sight.
   */
  it('ends a summary take when the conversation half goes', async () => {
    await draw();
    await openConversation();

    micIn('.rv-finishrow').click();
    await opened();
    push({ wire: record(TAKE), refused: null });
    flushSync();
    sent = [];

    const filesTab = [...document.querySelectorAll<HTMLButtonElement>('.rv-tabs > button')].find(
      (button) => button.textContent?.trim().startsWith('Files changed') === true,
    );
    if (filesTab === undefined) throw new Error('the page drew no Files tab');
    filesTab.click();
    flushSync();

    expect(sent, 'a summary take outlived the half that draws it').toEqual([
      { dictate_stop: { key: SLOT, submit: false } },
    ]);
  });

  /**
   * **A submitted take's words are never dropped silently.** With the record
   * transcribing, the reader closes the box; the words land afterwards, and
   * the box is opened again at the spot it began at so the reader sees them
   * instead of losing them silently.
   */
  it('opens the box again when the words land after it closed', async () => {
    await draw();
    await openComment();

    micIn('.rv-thread.comment').click();
    await opened();
    push({ wire: record(TAKE), refused: null });
    flushSync();
    // The finish: the release submits, and the capture drains before the
    // stop goes - the window the close below lands in.
    micIn('.rv-thread.comment').click();
    sent = [];

    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.rv-thread.comment .rv-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error('the comment box drew no cancel');
    cancel.click();
    await settle(() => document.querySelector('.rv-thread.comment') === null);

    // The drain runs on real timers: the stop that lands must be the
    // SUBMIT the reader chose, never the abandon a close would send.
    await vi.waitFor(
      () => {
        expect(wire(), 'the submitted take never reached the wire').toContain('"submit":true');
        expect(wire(), 'the close sent an abandon for a submitted take').not.toContain(
          '"submit":false',
        );
      },
      { timeout: 2000 },
    );

    push({ wire: record(TRANSCRIBING), refused: null });
    flushSync();
    push({ wire: record(LANDED), refused: null });
    flushSync();
    await settle(() => document.querySelector('.rv-thread.comment .rv-txarea') !== null);

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      'the words landed after the box closed and went nowhere the reader can see',
    ).toBe('this read is the whole input');
  });

  /**
   * **The reply reopens too, on the thread the take began on.** The comment
   * box's reopen is one branch of the same effect; a reply closed between
   * the release and the landing must come back on ITS thread, not another's
   * and not nowhere.
   */
  it('opens the reply box again on its thread when the words land after it closed', async () => {
    await draw([THREAD]);
    await openComment();
    await openReply();

    micIn('.rv-reply').click();
    await opened();
    push({ wire: record(TAKE, [THREAD]), refused: null });
    flushSync();
    micIn('.rv-reply').click();
    sent = [];

    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.rv-reply .rv-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error('the reply box drew no cancel');
    cancel.click();
    await settle(() => document.querySelector('.rv-reply') === null);
    await vi.waitFor(
      () => {
        expect(wire(), 'the submitted take never reached the wire').toContain('"submit":true');
      },
      { timeout: 2000 },
    );

    push({ wire: record(TRANSCRIBING, [THREAD]), refused: null });
    flushSync();
    push({ wire: record(LANDED, [THREAD]), refused: null });
    flushSync();
    await settle(() => document.querySelector('.rv-reply .rv-txarea') !== null);

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-reply .rv-txarea')?.value,
      'the words landed after the reply box closed and went nowhere the reader can see',
    ).toBe('this read is the whole input');
  });

  /** An install without dictation starts nothing from the key, either: the
   *  gate exists so a parked engine is never sent a take. */
  it('offers no key take when dictation is off', async () => {
    await draw([], [], false);
    await openConversation();

    window.dispatchEvent(
      new KeyboardEvent('keydown', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await opened();

    expect(wire(), 'a parked engine was sent a take').not.toContain('dictate_stream');
  });

  /**
   * **A MOVED box is not the take's box either.** Moving the reply to
   * another thread mid-take would file the words as an answer to a thread
   * the reader never dictated on, so the take ends with the move - the
   * document page's rule, on this page.
   */
  it('ends the take when its box moves to another thread', async () => {
    await draw([THREAD, SECOND]);
    await openComment();
    await openReply();

    micIn('.rv-reply').click();
    await opened();
    push({ wire: record(TAKE, [THREAD, SECOND]), refused: null });
    flushSync();
    sent = [];

    // The second thread's own respond: the box moves to it.
    const responds = [
      ...document.querySelectorAll<HTMLButtonElement>('tr.rv-insert button'),
    ].filter((button) => button.textContent?.trim() === 'respond');
    const second = responds[1];
    if (second === undefined) throw new Error('the canvas drew no second respond');
    second.click();
    flushSync();
    expect(
      document.querySelector('.rv-reply'),
      'the click closed the box rather than moving it, so nothing was tested',
    ).not.toBeNull();

    expect(sent, 'the take survived the move and would file under the wrong thread').toEqual([
      { dictate_stop: { key: SLOT, submit: false } },
    ]);
  });

  /** Escape is the other door out of a live take, wherever the focus is. */
  it('cancels a live take on Escape', async () => {
    await draw();
    await openComment();

    micIn('.rv-thread.comment').click();
    await opened();
    push({ wire: record(TAKE), refused: null });
    flushSync();
    sent = [];

    window.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }),
    );
    flushSync();

    expect(sent, 'Escape left the take running').toEqual([
      { dictate_stop: { key: SLOT, submit: false } },
    ]);
  });
});

describe("the record's line", () => {
  /**
   * **The record's line goes to the box that began the take.** A take that
   * fails server-side settles as a line on the record, and a line routed to
   * the comment box by default draws nothing at all for a take that failed in
   * the reply - whose box is not the comment box, and whose failure the
   * reader would then never see.
   */
  it("draws a failed take's line in the box that began it", async () => {
    await draw([THREAD]);
    await openComment();
    await openReply();

    micIn('.rv-reply').click();
    await opened();
    push({ wire: record(TAKE, [THREAD]), refused: null });
    flushSync();
    push({
      wire: record(
        {
          ...TAKE,
          take: null,
          notice: { kind: 'line', tone: 'bad', text: 'nothing above -60 dBFS in 3s' },
        },
        [THREAD],
      ),
      refused: null,
    });
    flushSync();

    expect(
      document.querySelector('.rv-reply .rv-dictateline')?.textContent,
      "the failed take's line, in the box that began it",
    ).toContain('nothing above -60 dBFS in 3s');
    expect(
      document.querySelector('.rv-thread.comment .rv-dictateline'),
      'and not in the comment box, which began nothing',
    ).toBeNull();
  });
});

describe("the key's box", () => {
  /**
   * **A key-begun take goes to a box that is DRAWN.** A comment spot outlives
   * the files tab; the take it would have claimed is the summary's, whose box
   * is on screen - never one the reader cannot see.
   */
  it('gives the key take to the drawn summary, not a reply the half does not draw', async () => {
    // The reply is open on an UNFILED thread: the files half draws it, the
    // conversation half draws no box for it at all.
    await draw([THREAD]);
    await openComment();
    await openReply();
    await openConversation();

    window.dispatchEvent(
      new KeyboardEvent('keydown', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await opened();

    push({ wire: record(TAKE), refused: null });
    flushSync();
    push({ wire: record(LANDED), refused: null });
    flushSync();
    expect(wire(), 'the key began no take at all').toContain('dictate_stream');

    await new Promise((resolve) => setTimeout(resolve, TAP_MS + 100));
    window.dispatchEvent(
      new KeyboardEvent('keyup', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.rv-finishrow .takeline'),
          'the bar goes with the take',
        ).toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLInputElement>('.rv-summary')?.value,
      'the key take went into a reply box the half does not draw',
    ).toBe('this read is the whole input');
  });

  it('gives the key take to the drawn summary, not a stale comment spot', async () => {
    await draw();
    await openComment();
    await openConversation();

    window.dispatchEvent(
      new KeyboardEvent('keydown', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await opened();

    push({ wire: record(TAKE), refused: null });
    flushSync();
    push({ wire: record(LANDED), refused: null });
    flushSync();
    expect(wire(), 'the key began no take at all').toContain('dictate_stream');

    // The release ends the take, and the bar goes with it - on the drain's
    // real timers (#2159), which is what puts the field back in the
    // summary's line for the assertion to read. The hold has to clear
    // TAP_MS, or the release is a tap and means nothing.
    await new Promise((resolve) => setTimeout(resolve, TAP_MS + 100));
    window.dispatchEvent(
      new KeyboardEvent('keyup', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.rv-finishrow .takeline'),
          'the bar goes with the take',
        ).toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLInputElement>('.rv-summary')?.value,
      'the key take went somewhere the reader cannot see',
    ).toBe('this read is the whole input');
  });
});

describe("the conversation half's reply box", () => {
  /**
   * **A box is a box on either half.** The conversation half's reply takes a
   * take from its own mic, and the words land APPENDED - what the reader
   * typed before the press survives the landing.
   */
  it('takes a take from its own mic, after what was typed', async () => {
    await draw([HELD_THREAD], [ROUND]);
    await openConversation();

    const respond = [...document.querySelectorAll<HTMLButtonElement>('.thread .acts button')].find(
      (button) => button.textContent?.trim() === 'respond',
    );
    if (respond === undefined) throw new Error('the conversation half drew no respond');
    respond.click();
    await settle(() => document.querySelector('.rv-reply') !== null);

    const box = document.querySelector<HTMLTextAreaElement>('.rv-reply .rv-txarea');
    if (box === null) throw new Error('the reply box drew no field');
    box.value = 'noted: ';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    await dictateInto('.rv-reply', [HELD_THREAD], [ROUND]);

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-reply .rv-txarea')?.value,
      "the take's words were not appended to what was already typed",
    ).toBe('noted: this read is the whole input');
  });
});
