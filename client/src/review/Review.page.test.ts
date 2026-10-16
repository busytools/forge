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

const viewer = vi.hoisted(() => ({ built: [] as string[] }));

vi.mock('@git-diff-view/core', () => ({
  // The viewer's own model, stood in for the way the component is: what a
  // test reads is the diff TEXT each build was handed, which is where an
  // opened gap either reached the viewer or did not.
  DiffFile: class {
    constructor(...args: unknown[]) {
      const hunks = args[4];
      viewer.built.push(Array.isArray(hunks) ? hunks.join('') : '');
    }
  },
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
import { TRUNCATED } from '../composer/view';
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
  // jsdom performs no layout: scrollIntoView is a browser call the jump and
  // the comment stepper make, so it is stood in for the way the board and
  // the chat's own reveal stand it in.
  Object.defineProperty(Element.prototype, 'scrollIntoView', {
    configurable: true,
    value: () => {},
  });
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
  diff: unknown = DIFF,
  commits: { sha: string; subject: string }[] = [],
): SessionRecord {
  return {
    slot: SLOT,
    composer,
    work: { branch: 'b', dirt: 'clean' },
    git: {
      defaultBranch: 'main',
      ahead: commits.length === 0 ? null : { commit_count: commits.length, commits, stats: null },
    },
    diff: { branchAhead: diff },
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
  notice: { kind: 'landed', text: 'this read is the whole input', truncated: false, generation: 1 },
};

/** A second take's landing, with words of its own - or the same ones again. */
const landedAgain = (text: string, generation: number): unknown => ({
  ...TAKE,
  take: null,
  notice: { kind: 'landed', text, truncated: false, generation },
});

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
/** The file-content asks the canvas sent, and the listeners a test answers
 *  them through. */
let asked: { path: string; sha: string | null }[] = [];
const hearers = new Set<(message: unknown) => void>();

/** Hand the page the frame the server would have sent. */
function pushMessage(message: unknown): void {
  for (const hear of [...hearers]) hear(message);
}

function connection(): Connection {
  return {
    settings: () => null,
    dispatch: (command?: unknown) => {
      sent.push(command);
      // A dispatch's answer, from the few writes that read one: the seal
      // reads a round back (inert for every other command).
      return { id: 'r9', number: 4, summary: null, created_at: '2026-01-01T00:00:00Z' };
    },
    refresh: () => undefined,
    commitDiff: () => undefined,
    fileContent: (_session: unknown, path: string, sha: string | null) => {
      asked.push({ path, sha });
      return true;
    },
    status: () => 'open',
    onStatus: () => () => undefined,
    onMessage: (fn: (message: unknown) => void) => {
      hearers.add(fn);
      return () => {
        hearers.delete(fn);
      };
    },
  } as unknown as Connection;
}

/** The page over a seat whose record the test drives, drawn to the point its
 *  diff's own doors are on screen. */
async function draw(
  threads: unknown[] = [],
  reviews: unknown[] = [],
  dictation = true,
  diff: unknown = DIFF,
  commits: { sha: string; subject: string }[] = [],
): Promise<void> {
  sent = [];
  asked = [];
  hearers.clear();
  viewer.built = [];
  app = mount(Review, {
    target: document.body,
    props: { connection: connection(), slot: SLOT, dictation },
  });
  flushSync();
  push({ wire: record(IDLE, threads, reviews, diff, commits), refused: null });
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

  /**
   * **The seat's carried words are the composer's to place, not this page's**
   * (#1360), even after this page has had a take of its own: the guard reads
   * whether a take is in flight, not the last box the page began - a stale
   * box must not be re-opened for words that are the composer's, nor draw the
   * truncated warn row about them.
   */
  it('draws nothing for a carried notice after this page has had a take', async () => {
    await draw();
    await openComment();
    await dictateInto('.rv-thread.comment');
    push({ wire: record(LANDED), refused: null });
    flushSync();

    push({
      wire: record({
        ...IDLE,
        notice: {
          kind: 'landed',
          text: 'the unsent words',
          truncated: true,
          carried: true,
          generation: 4,
        },
      }),
      refused: null,
    });
    flushSync();

    expect(
      document.body.textContent,
      'no warn row about words this page cannot hold',
    ).not.toContain(TRUNCATED);
    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value ?? '',
      'and the stale box was not opened with words the composer owns',
    ).not.toContain('the unsent words');
  });

  /**
   * **A parked row is not a dead end** (Ved, 2026-10-15). The Outdated rows
   * used to draw no actions at all - no respond, no resolve - so the one
   * thread the resolver could not place was the one row with no door back.
   * Here the thread is held by no round, so the reply box - which draws only
   * under a round's own copy - has nowhere to open: resolve is the door.
   */
  it('offers resolve on a parked thread no round holds', async () => {
    const parked = {
      id: 'parked-1',
      anchor: {
        path: 'src/a.rs',
        side: 'New',
        line: 9,
        content_hash: '1',
        context: ['captured one', 'captured two'],
        base_ref: 'main',
      },
      comments: [
        {
          author: { Agent: { label: 'w1' } },
          text: 'still open',
          at: '2026-01-01T00:00:00Z',
        },
      ],
      status: 'Open',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
      commit: null,
      resolution: null,
    };
    await draw([parked]);
    await openConversation();

    const row = document.querySelector('.thread.parked');
    expect(row, 'the thread parked').not.toBeNull();
    const acts = [...(row?.querySelectorAll('.acts .rv-a') ?? [])].map((button) =>
      button.textContent?.trim(),
    );
    expect(acts, 'with resolve, and no respond the page could not honour').toEqual(['resolve']);

    sent = [];
    [...(row?.querySelectorAll<HTMLButtonElement>('.acts .rv-a') ?? [])][0]?.click();
    flushSync();
    expect(sent, 'and resolve writes the state for the parked thread').toEqual([
      {
        set_review_thread_status: {
          project: 'forge',
          branch: 'b',
          thread_id: 'parked-1',
          status: 'Resolved',
        },
      },
    ]);
  });

  it('offers respond on a parked thread a round holds', async () => {
    // The parked copy of a thread filed into ROUND: respond is offered
    // because the reply box it opens draws under that round's own copy.
    const parked = {
      id: 'parked-1',
      anchor: {
        path: 'src/a.rs',
        side: 'New',
        line: 9,
        content_hash: '1',
        context: ['captured one', 'captured two'],
        base_ref: 'main',
      },
      comments: [
        {
          author: { Agent: { label: 'w1' } },
          text: 'still open',
          at: '2026-01-01T00:00:00Z',
          review_id: 'r1',
        },
      ],
      status: 'Open',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
      commit: null,
      resolution: null,
    };
    await draw([parked], [ROUND]);
    await openConversation();

    const row = document.querySelector('.thread.parked');
    expect(row, 'the thread parked').not.toBeNull();
    const acts = [...(row?.querySelectorAll('.acts .rv-a') ?? [])].map((button) =>
      button.textContent?.trim(),
    );
    expect(acts, 'respond rides along where the round draws the box').toEqual([
      'respond',
      'resolve',
    ]);

    [...(row?.querySelectorAll<HTMLButtonElement>('.acts .rv-a') ?? [])][0]?.click();
    flushSync();
    expect(
      document.querySelector('.rv-reply'),
      'and the reply box opens under the round that holds it',
    ).not.toBeNull();
  });

  /**
   * **Done closes the round** (Ved, 2026-10-15): a landed seal closes the
   * comment and reply boxes and clears the summary - the page is back to the
   * diff with nothing of the round left open. The mock answers the seal with
   * a round, which is what `finishRound` reads to call it landed (inert for
   * every other case, which never reads a dispatch's answer).
   */
  it('closes the round when Done seals it', async () => {
    const live = {
      id: 't-open',
      anchor: {
        path: 'src/a.rs',
        side: 'New',
        line: 1,
        content_hash: '1',
        context: ['one'],
        base_ref: 'main',
      },
      comments: [
        {
          author: { Agent: { label: 'w1' } },
          text: 'answered words',
          at: '2026-01-01T00:00:00Z',
          review_id: 'r1',
        },
      ],
      status: 'Addressed',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
      commit: null,
      resolution: { state: 'placed', line: 1, moved_from: null },
    };
    await draw([live], [ROUND]);
    await openComment();

    const box = document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea');
    if (box === null) throw new Error('the comment box drew no field');
    box.value = 'a drafted comment';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    const file = [
      ...document.querySelectorAll<HTMLButtonElement>('.rv-thread.comment button'),
    ].find((button) => button.textContent?.trim() === 'comment');
    if (file === undefined) throw new Error('the comment box drew no comment action');
    file.click();
    await settle(() => sent.some((held) => typeof held === 'object' && held !== null));

    await openReply();
    await openConversation();
    // The precondition the seal's own assert rests on: the conversation half
    // is where the reply box is left, and only a round that holds the thread
    // draws it - without this the assert below passes on a box never drawn.
    expect(
      document.querySelector('.rv-reply'),
      'precondition: the reply box is open on the conversation half',
    ).not.toBeNull();

    const summaryField = document.querySelector<HTMLInputElement>('.rv-summary');
    if (summaryField === null) throw new Error('the round drew no summary field');
    summaryField.value = 'the round summary';
    summaryField.dispatchEvent(new Event('input', { bubbles: true }));
    summaryField.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
    );
    await settle(() => document.body.textContent?.includes('the round summary') === false);

    expect(
      sent.some((held) => typeof held === 'object' && held !== null && 'submit_review' in held),
      'the seal went out',
    ).toBe(true);
    expect(document.querySelector('.rv-reply'), 'the reply box closed with the round').toBeNull();
    expect(
      document.querySelector<HTMLInputElement>('.rv-summary')?.value ?? '',
      'and the summary is cleared',
    ).toBe('');
    // The comment box's own clears in `finish()` are not observable from
    // here: the seal runs on the conversation half, and the canvas closes
    // the box when it mounts on the return. They earn their place the day a
    // Done door draws on the files half.
  });

  /**
   * **A landing the record re-hands is not the new take's.** Any write between
   * the press and the take's own start runs the landing with the PREVIOUS
   * notice still on the record: skipping that notice rather than spending the
   * arm on it is what lets the new take's words land (the gate's F1,
   * 2026-10-15).
   */
  it("lands a second take's words with the first landing re-read at its press", async () => {
    await draw();
    await openComment();
    await dictateInto('.rv-thread.comment');

    // The second take's press, with the first landing still on the record -
    // and a write that hands that same notice over again while it runs.
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
    push({ wire: record(LANDED), refused: null });
    flushSync();

    push({ wire: record(landedAgain('and this one lands too', 2)), refused: null });
    flushSync();

    micIn('.rv-thread.comment').click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.rv-thread.comment .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      "the second take's words, at the end of the first landing",
    ).toContain('and this one lands too');
  });

  /**
   * **The same words spoken twice are two landings.** The record hands a
   * notice over on every write, so the take's own number is what tells the
   * first landing re-read from a second take that spoke the same words - and
   * the second take here is quick, so no write clears the first notice before
   * its own lands (the gate's F2, 2026-10-15).
   */
  it('lands the same words again when another take spoke them', async () => {
    await draw();
    await openComment();
    await dictateInto('.rv-thread.comment');

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
    // The second take lands the same words, its take's own number and all.
    push({ wire: record(landedAgain('this read is the whole input', 2)), refused: null });
    flushSync();

    micIn('.rv-thread.comment').click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.rv-thread.comment .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      'the same words twice, as two takes spoke them',
    ).toBe('this read is the whole input this read is the whole input');
  });
});

describe('a comment box opens', () => {
  /**
   * **The caret goes in with the box** (Ved, 2026-10-16): clicking a line to
   * comment is a reader asking to write there, and a box that opened
   * unfocused made them click a second time before a word could be typed.
   */
  it('puts the caret in the field the click opened', async () => {
    await draw();
    await openComment();

    expect(document.activeElement, 'the caret is in the box the click opened').toBe(
      document.querySelector('.rv-thread.comment .rv-txarea'),
    );
  });
});

describe('the steppers', () => {
  const SECOND_COMMIT = { sha: 'def456', subject: 'the second commit' };

  /** The readout the comment stepper draws, or null when it drew none. */
  function readout(): string | null {
    return document.querySelector('.rv-cstep-n')?.textContent?.trim() ?? null;
  }

  function press(where: 'previous comment' | 'next comment' | 'next scope'): void {
    const button = document.querySelector<HTMLButtonElement>(`.rv-step-btn[aria-label="${where}"]`);
    if (button === null) throw new Error(`the page drew no ${where} button`);
    button.click();
    flushSync();
  }

  /**
   * **Both steppers cycle, with buttons a finger can press** (Ved,
   * 2026-10-16): the comments of the scope, and the scopes themselves - the
   * last steps round to the first rather than stopping at a wall, and the
   * pair is on screen so a touch reader has a door the keys are not.
   */
  it('walks the scope comments with wrap-around, from its own buttons', async () => {
    await draw([
      THREAD,
      {
        ...THREAD,
        id: 't4',
        resolution: { state: 'placed', line: 4, moved_from: null },
        anchor: { ...THREAD.anchor, line: 4 },
      },
    ]);

    expect(readout(), 'nothing stepped to yet').toBe('0/2');
    press('next comment');
    expect(readout()).toBe('1/2');
    press('next comment');
    expect(readout()).toBe('2/2');
    press('next comment');
    expect(readout(), 'the last comment steps round to the first').toBe('1/2');
    press('previous comment');
    expect(readout(), 'and the first steps back round to the last').toBe('2/2');
  });

  it('steps the scopes with wrap-around, from its own buttons', async () => {
    await draw([], [], true, DIFF, [{ sha: 'abc123', subject: 'the first commit' }, SECOND_COMMIT]);

    const scope = (): string | null =>
      document.querySelector('.rv-scope-now')?.textContent?.trim() ?? null;
    expect(scope()).toBe('All changes');
    press('next scope');
    expect(scope()).toBe('abc123');
    press('next scope');
    expect(scope()).toBe('def456');
    press('next scope');
    expect(scope(), 'the last commit steps round to all changes').toBe('All changes');
  });

  /**
   * **The keys are the pair the hint names**: arrows step commits, and
   * shift+arrows step comments - the same walks the buttons make.
   */
  it('steps comments on shift+arrows and commits on the arrows', async () => {
    const key = (init: KeyboardEventInit): void => {
      document.body.dispatchEvent(
        new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init }),
      );
      flushSync();
    };
    await draw([THREAD], [], true, DIFF, [SECOND_COMMIT]);

    key({ key: 'ArrowRight', shiftKey: true });
    expect(readout(), 'shift+right steps a comment').toBe('1/1');
    key({ key: 'ArrowLeft', shiftKey: true });
    expect(readout(), 'and shift+left steps back round').toBe('1/1');

    const scope = (): string | null =>
      document.querySelector('.rv-scope-now')?.textContent?.trim() ?? null;
    key({ key: 'ArrowRight' });
    expect(scope(), 'a bare right still steps the scope').toBe('def456');

    expect(document.querySelector('.rv-hint')?.textContent, 'and the hint row says both').toContain(
      'shift',
    );
  });
});

describe('the files half finishes the round too', () => {
  /** File one comment on the diff, so the round has a draft to seal. */
  async function draft(): Promise<void> {
    await openComment();
    const box = document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea');
    if (box === null) throw new Error('the comment box drew no field');
    box.value = 'a drafted comment';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    const file = [
      ...document.querySelectorAll<HTMLButtonElement>('.rv-thread.comment button'),
    ].find((button) => button.textContent?.trim() === 'comment');
    if (file === undefined) throw new Error('the comment box drew no comment action');
    file.click();
    await settle(() => sent.some((held) => typeof held === 'object' && held !== null));
  }

  /**
   * **The reader finishes without leaving the diff** (Ved, 2026-10-16): a
   * `finish review (N)` control in the files half opens the page's own
   * finish row in place, the summary field takes the caret, and Enter seals
   * the round - after which the row collapses back to its control.
   */
  it('opens the row on a press, takes the caret, and seals on Enter', async () => {
    await draw();
    await draft();

    const button = document.querySelector<HTMLButtonElement>('.rv-finishbar .rv-finish');
    expect(button, 'the files half drew its finish control').not.toBeNull();
    expect(button?.textContent?.trim(), 'naming the drafts it would seal').toContain('(1)');

    button?.click();
    await settle(() => document.querySelector('.rv-finishbar .rv-summary') !== null);

    expect(document.activeElement, 'the field a press opened takes the caret').toBe(
      document.querySelector('.rv-finishbar .rv-summary'),
    );

    sent = [];
    const field = document.querySelector<HTMLInputElement>('.rv-finishbar .rv-summary');
    if (field === null) throw new Error('the files half drew no summary field');
    field.value = 'the round summary';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    field.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
    );
    await settle(() => document.querySelector('.rv-finishbar .rv-summary') === null);

    expect(
      sent.some((held) => typeof held === 'object' && held !== null && 'submit_review' in held),
      'the seal the conversation half runs, from the files half',
    ).toBe(true);
    expect(
      document.querySelector('.rv-finishbar .rv-finish'),
      'and the row collapsed back to its control',
    ).not.toBeNull();
  });
});

describe('the lines a diff left out', () => {
  /** Two hunks round the stood-in viewer's two carried lines (1 and 4): the
   *  gap between them hides lines 2 and 3. */
  const TWO_HUNKS = {
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
          {
            oldStart: 4,
            oldCount: 1,
            newStart: 4,
            newCount: 1,
            lines: [{ kind: 'context', text: 'four', oldLine: 4, newLine: 4 }],
          },
        ],
      },
    ],
  };

  /** The file's whole content, as the read would answer it. */
  const LINES = ['one', 'two', 'three', 'four'];

  /**
   * **The read is asked when the file is drawn**, not when a control is
   * pressed: the answer is what decides whether a control exists at all, so
   * a door that would open on a read that cannot fill it is never drawn.
   */
  it('asks for the file the canvas draws, and draws the control its answer earns', async () => {
    await draw([], [], true, TWO_HUNKS);

    expect(
      asked.map((held) => held.path),
      'the canvas asked for the drawn file',
    ).toEqual(['src/a.rs']);
    expect(asked[0]?.sha, 'in the all-changes scope, against the branch tip').toBeNull();
    expect(
      document.querySelector('.rv-expand'),
      'no control before the read landed: a door nothing could fill yet',
    ).toBeNull();

    pushMessage({
      kind: 'file_content',
      session: SLOT,
      path: 'src/a.rs',
      sha: null,
      content: { state: 'read', lines: LINES, truncated: false },
    });
    await settle(() => document.querySelector('.rv-expand') !== null);

    const control = document.querySelector('.rv-expand');
    expect(control?.textContent, 'the control names how much it hides').toBe('show 2 lines');
    expect(
      document.querySelector('.rv-expand')?.closest('tr.rv-insert'),
      'and it sits in a row of its own, after the first hunk',
    ).not.toBeNull();

    // **The press re-emits the file's diff text with the gap drawn as
    // context**, which is the only way the viewer can number those rows: a
    // heading that kept its old counts would number every hunk below wrong.
    control?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await settle(() => document.querySelector('.rv-expand') === null);

    const text = viewer.built.at(-1) ?? '';
    expect(text, 'the gap is drawn in the hunk it follows').toContain(
      '@@ -1,3 +1,3 @@\n one\n two\n three\n@@ -4,1 +4,1 @@\n four\n',
    );
  });

  /** A read that refused earns no control: the page draws no door onto a
   *  read the server answers with a refusal. */
  it('draws no control for a file whose read refused', async () => {
    await draw([], [], true, TWO_HUNKS);

    pushMessage({
      kind: 'file_content',
      session: SLOT,
      path: 'src/a.rs',
      sha: null,
      content: { state: 'failed', why: 'no file at src/a.rs in HEAD' },
    });
    await settle(() => asked.length > 0);

    expect(document.querySelector('.rv-expand'), 'no door onto a refused read').toBeNull();
  });

  /**
   * **A line the hunks do not carry takes no door.** An expanded gap draws
   * lines the record does not hold, and a thread's anchor is resolved
   * against what the server ships: a comment written on one would park as
   * outdated the moment it was filed, so the page does not offer.
   */
  it('draws no door, and opens no box, on a line the hunks do not carry', async () => {
    await draw();

    const rows = [...document.querySelectorAll('tr.diff-line')];
    const uncarried = rows.find((row) => row.querySelector('span[data-line-new-num="9"]') !== null);
    expect(uncarried, 'the stood-in viewer drew the uncarried line').toBeDefined();
    expect(uncarried?.querySelector('button.rv-add'), 'no door on it').toBeNull();

    uncarried?.querySelector<HTMLElement>('td.diff-line-content')?.click();
    flushSync();
    expect(
      document.querySelector('.rv-thread.comment'),
      'and a click on it opens no box',
    ).toBeNull();
  });
});

describe('a scope and its threads', () => {
  /** A comment written inside a commit, whose line the branch still carries -
   *  the server resolved it there, by content. */
  const SCOPED = {
    ...THREAD,
    id: 't-scoped',
    commit: 'abc123',
    comments: [
      { author: { kind: 'user' }, text: 'written inside a commit', at: 'x', review_id: null },
    ],
    resolution: { state: 'placed', line: 4, moved_from: null },
  };
  /** And one the branch no longer shows where it was. */
  const SCOPED_GONE = {
    ...THREAD,
    id: 't-scoped-gone',
    commit: 'abc123',
    comments: [
      { author: { kind: 'user' }, text: 'the branch lost this one', at: 'x', review_id: null },
    ],
    resolution: { state: 'outdated', reason: 'gone' },
  };

  /**
   * **A thread written inside a commit draws in all changes too**, where the
   * branch's diff still carries its line: the resolver placed it by content,
   * and the page draws it at that landing rather than hiding it in the
   * commit's own scope.
   */
  it('draws a comment written inside a commit on the all-changes canvas', async () => {
    await draw([SCOPED]);
    await settle(() => document.querySelector('tr.rv-insert') !== null);

    expect(
      document.querySelector('tr.rv-insert')?.textContent,
      'the commit-scoped thread, on the branch canvas',
    ).toContain('written inside a commit');
  });

  /**
   * **A commit thread the branch no longer carries parks rather than being
   * guessed onto its commit-numbered line** - that number addresses
   * unrelated code in this numbering, and its own commit's view still draws
   * it where it was written.
   */
  it('parks a commit thread the branch no longer carries', async () => {
    await draw([SCOPED_GONE]);
    await openConversation();

    const parked = document.querySelector('.thread.parked');
    expect(parked, 'the thread the branch cannot place parked').not.toBeNull();
    expect(parked?.textContent, 'with its own words').toContain('the branch lost this one');
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
   * **The key speaks in every box on this page** (Ved, 2026-10-16): the
   * dictation key starts a take in the box the reader is in - the comment
   * box on the diff and the reply box under a thread, as well as the round's
   * summary - over the same registration the composer's own box uses.
   */
  it('starts a take in the comment box the reader is in', async () => {
    await draw();
    await openComment();

    window.dispatchEvent(
      new KeyboardEvent('keydown', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await opened();
    expect(wire(), 'the key began no take at all').toContain('dictate_stream');

    push({ wire: record(TAKE), refused: null });
    flushSync();
    push({ wire: record(LANDED), refused: null });
    flushSync();
    // The hold has to clear TAP_MS, or the release is a tap and means
    // nothing (the composer's own rule).
    await new Promise((resolve) => setTimeout(resolve, TAP_MS + 100));
    window.dispatchEvent(
      new KeyboardEvent('keyup', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await vi.waitFor(
      () => {
        expect(document.querySelector('.rv-thread.comment .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-thread.comment .rv-txarea')?.value,
      'the key take went somewhere other than the box the reader was in',
    ).toBe('this read is the whole input');
  });

  it('starts a take in the reply box the reader is in', async () => {
    await draw([THREAD]);
    await openComment();
    await openReply();
    // The reader's caret is what names the box: clicking into the reply is
    // what says they are dictating there rather than in the comment box
    // still open beside it.
    document.querySelector<HTMLTextAreaElement>('.rv-reply .rv-txarea')?.focus();

    window.dispatchEvent(
      new KeyboardEvent('keydown', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await opened();
    expect(wire(), 'the key began no take at all').toContain('dictate_stream');

    push({ wire: record(TAKE, [THREAD]), refused: null });
    flushSync();
    push({ wire: record(LANDED, [THREAD]), refused: null });
    flushSync();
    await new Promise((resolve) => setTimeout(resolve, TAP_MS + 100));
    window.dispatchEvent(
      new KeyboardEvent('keyup', { code: 'ControlRight', bubbles: true, cancelable: true }),
    );
    await vi.waitFor(
      () => {
        expect(document.querySelector('.rv-reply .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    expect(
      document.querySelector<HTMLTextAreaElement>('.rv-reply .rv-txarea')?.value,
      'the key take went somewhere other than the reply box the reader was in',
    ).toBe('this read is the whole input');
  });

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
