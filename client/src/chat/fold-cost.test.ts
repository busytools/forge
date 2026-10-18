// @vitest-environment jsdom
/**
 * The live turn's fold, as a COUNT.
 *
 * #1643: every arriving frame rebuilt `turn.messages` as a new array, so the
 * whole turn was re-folded per frame the column drew - `Turn.svelte`'s
 * `folded` and `Chat.svelte`'s `newestRow` - and a turn of M frames re-read
 * its own frames ~2M^2 times over its life. The defect is a cost, so the pins
 * here are counts of the frames the fold READS (`counted`), not drawings: an
 * arriving counter must cost its own frame rather than the turn, and one
 * arriving frame must be folded once rather than once per reader.
 *
 * The count is taken off the frames themselves rather than out of the fold,
 * so it measures the work whatever the fold's shape is - the same seam as the
 * list stub's `scrollHeight` read count (#1710) - and a fold that stopped
 * carrying its answer is counted again at the turn's size.
 *
 * The drawing is pinned beside it: `turnFold`'s answer must equal `fold`'s,
 * frame for frame, which is the guard that the carry is a speed change and
 * nothing else.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ClientMessage, ServerMessage, SessionUpdate } from '../protocol';
import type { Connection } from '../socket';
import { Stores } from '../stores';
import { git } from './git.svelte';
import { pinnedRow } from './pinnedRow.svelte';
import { subjectKey } from '../protocol';
import type { SessionSlot } from '../wire/types';
import Chat from './Chat.svelte';
import { installResizeObserver } from './testing/viewport';
import { subagents } from './subagents.svelte';
import { fold, turnFold } from './units';
import { dispatchTimeline } from './timeline';

installResizeObserver();

vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

/**
 * What a dispatch row's timeline reader is handed.
 *
 * The seam catches a copy made BETWEEN the page and here, whatever its
 * spelling (`flatMap`, a spread, `concat`, `Array.from`) - it would arrive as
 * a flattened messages array rather than the page's turns. What the reader
 * does with it is pinned where it lives (`timeline.test.ts`).
 */
const seam = vi.hoisted(() => ({ seen: [] as unknown[] }));

vi.mock('./timeline', async (importOriginal) => {
  const real = await importOriginal<typeof import('./timeline')>();
  return {
    ...real,
    dispatchTimeline: (turns: Parameters<typeof real.dispatchTimeline>[0], id: string) => {
      seam.seen.push(turns);
      if (!turns.every((turn) => Array.isArray(turn.messages))) {
        throw new Error('the timeline reader was handed a flattened history, not the page turns');
      }
      return real.dispatchTimeline(turns, id);
    },
  };
});

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const PIN = subjectKey({ session: LEAD });

/** Every top-level field read off the turn's own frames, which a fold costs. */
let reads = 0;

/**
 * A frame whose own fields count their reads.
 *
 * The frames are what a fold re-reads, whatever the fold is made of, so the
 * count follows a re-fold wherever it lives. Wrapped ONCE per frame and kept,
 * so identity - which the carry keys on - is what it was.
 */
function counted<T extends object>(frame: T): T {
  return new Proxy(frame, {
    get(target, key, receiver) {
      reads += 1;
      return Reflect.get(target, key, receiver);
    },
  });
}

const prompt = (text: string): unknown =>
  counted({
    type: 'user',
    uuid: `u-${text}`,
    message: { role: 'user', content: [{ type: 'text', text }] },
  });

/** The storm's own frame: a counter append that draws nothing of its own. */
const counter = (at: number, timestamp?: string): unknown =>
  counted({
    type: 'system',
    subtype: 'thinking_tokens',
    uuid: `t-${at}`,
    estimated_tokens_delta: 12,
    ...(timestamp === undefined ? {} : { timestamp }),
  });

/** Whether a frame is one of the storm's: what a counter's arrival may cost. */
const isCounter = (frame: unknown): boolean =>
  (frame as { subtype?: unknown } | null)?.subtype === 'thinking_tokens';

/** One assistant frame, whose row the turn does have to draw. */
const said = (at: number): unknown =>
  counted({
    type: 'assistant',
    uuid: `a-${at}`,
    timestamp: '2026-10-01T06:00:00Z',
    message: {
      id: `m-${at}`,
      role: 'assistant',
      model: 'claude-opus-5',
      content: [{ type: 'text', text: `step ${at}` }],
      usage: { input_tokens: 100 + at, output_tokens: 20 },
    },
  });

/** The order a live turn's frames arrive in: a prompt, then a storm's worth. */
function arriving(size: number): unknown[] {
  const frames = [prompt('work')];
  for (let at = 0; at < size; at += 1) frames.push(at % 20 === 0 ? said(at) : counter(at));
  return frames;
}

/** The frames of `frames` after the first, one at a time, as the store grows them. */
function growing(frames: readonly unknown[]): Array<{ messages: unknown[] }> {
  const steps: Array<{ messages: unknown[] }> = [];
  let messages: unknown[] = [frames[0]];
  for (const frame of frames.slice(1)) {
    messages = [...messages, frame];
    steps.push({ messages });
  }
  return steps;
}

/**
 * Fold every step, as the column does, and answer what the counter-appended
 * ones cost between them.
 *
 * The counters are the storm, and a mean over every step would carry the
 * row-drawing frames' own whole folds in: what they cost is not what this pin
 * is about, and folding them keeps the carry current for the counters after
 * them - the same order the column folds in.
 */
function counterSteps(steps: readonly { messages: unknown[] }[]): {
  spent: number;
  counted: number;
} {
  let spent = 0;
  let counted = 0;
  for (const turn of steps) {
    reads = 0;
    turnFold(turn, null, true, false);
    const last = turn.messages[turn.messages.length - 1];
    if (!isCounter(last)) continue;
    spent += reads;
    counted += 1;
  }
  return { spent, counted };
}

describe('the live turn fold cost', () => {
  /**
   * The storm's shape: 4,140 of a live capture's 4,167 frames were counter
   * appends, and each one re-folded the whole turn. A counter carries nothing
   * the turn has to be re-read for, so it may cost its own frame's worth.
   *
   * **Only the counter-appended steps are measured.** The turn also carries
   * row-drawing frames, and each of those folds the whole turn by design - so
   * a mean over every step would be that cost in disguise, moving with how
   * many row-drawing frames the corpus happens to hold.
   */
  it('an arriving counter costs the frame, not the turn', () => {
    const frames = arriving(400);
    reads = 0;
    fold(frames, null, true, false);
    const whole = reads;
    expect(whole, 'a whole fold of the turn reads it').toBeGreaterThan(frames.length);
    const { spent, counted } = counterSteps(growing(frames));
    const perFrame = spent / counted;
    // A counter costs a fixed handful of reads against the twentieth of the
    // turn this allows, while the shape this pins away reads about two turns
    // per counter - some forty times over, and growing with the turn.
    expect(perFrame, 'frame reads per arriving counter').toBeLessThan(whole / 20);
  });

  /**
   * Both readers of one fold - the turn's own rows and the column's pinned
   * row - fold the same turn in the same paint. The second is owed the answer
   * rather than a second fold, whatever the frame arriving drew.
   */
  it.each([
    ['a counter', (at: number) => counter(at)],
    ['a row', (at: number) => said(at)],
  ])('folds one arriving %s once for its two readers', (_what, frame) => {
    const steps = growing(arriving(200));
    const first = steps[steps.length - 1] as { messages: unknown[] };
    first.messages = [...first.messages, frame(9000)];
    // The whole fold per reader, for the frame that draws: what two readers
    // would cost if neither was answered from the carry.
    reads = 0;
    fold(first.messages, null, true, false);
    const whole = reads;
    reads = 0;
    turnFold(first, null, true, false);
    const one = reads;
    reads = 0;
    turnFold(first, null, true, false);
    const two = reads;
    expect(one + two, 'reads for the frame, both readers').toBeLessThan(whole * 1.6);
    expect(two, 'the second reader reads the frame, not the turn').toBeLessThan(whole / 10);
  });
});

/** A connection a column is mounted over, and the frames it can be handed. */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const stores = new Stores();
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'loading' }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: (_message: ClientMessage) => null,
    more: () => true,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    browserRole: () => false,
    onBrowserRole: () => () => undefined,
    onProfilesChanged: () => () => undefined,
    takeBrowserRole: () => undefined,
    store: (what: Parameters<Connection['store']>[0]) => stores.get(what),
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;
  return {
    connection,
    send(message: ServerMessage): void {
      for (const fn of listeners) fn(message);
      flushSync();
    },
  };
}

async function painted(): Promise<void> {
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  flushSync();
}

const appended = (msg: unknown): ServerMessage => ({
  kind: 'update',
  update: { chat_appended: { key: LEAD, msg } } as SessionUpdate,
});

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  git.sync(null);
  pinnedRow.set(PIN, null);
  subagents.sync(null);
});

describe('the column as the frames arrive', () => {
  /**
   * The wiring: a mounted column draws the newest turn through the carried
   * fold, so an arriving counter does not send the turn through the fold
   * again - what a reader would pay per frame is the DRAW's own cost, not the
   * turn's.
   *
   * The budget is one whole fold of the turn, which the draw itself stays
   * under; a column folding per frame (the turn's own and the pinned row's,
   * as it did) costs two on top of the draw and fails it.
   */
  it('does not re-fold the live turn per arriving frame', async () => {
    const size = 200;
    const frames = arriving(size);
    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.send({ kind: 'page', conversation: LEAD, turns: [], cursor: null });
    await painted();
    server.send(appended(frames[0]));
    for (const frame of frames.slice(1)) server.send(appended(frame));
    await painted();
    reads = 0;
    fold(frames, null, true, false);
    const whole = reads;
    const FRAMES = 12;
    let spent = 0;
    for (let at = 0; at < FRAMES; at += 1) {
      const frame = counter(1000 + at);
      const before = reads;
      server.send(appended(frame));
      await painted();
      spent += reads - before;
    }
    // A probe that can only return zero would pass the bound below whatever
    // the column did, so the column's own reads have to be there at all.
    expect(spent, 'the column reads its frames').toBeGreaterThan(0);
    expect(spent / FRAMES, "the column's reads per arriving counter").toBeLessThan(whole);
  });

  /**
   * The scans OUTSIDE the fold (#2326).
   *
   * #1643 took the whole-turn re-fold out of the live path, and the same shape
   * survived beside it, each growing with the turn while the fold no longer
   * did. Measured here at a 400-frame turn, per arriving counter: the store's
   * duplicate check ~832 frame reads, the ask of whether the newest turn
   * carries its ending ~405, and the agents list's reachability walk ~813 -
   * against a whole fold of the turn at ~7,088.
   *
   * All three are per-turn lookups now, so what a counter costs is its own
   * frame's worth.
   */
  it('does not walk the held turns for a counter that draws nothing', async () => {
    const size = 400;
    const frames = arriving(size);
    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.send({ kind: 'page', conversation: LEAD, turns: [], cursor: null });
    await painted();
    server.send(appended(frames[0]));
    for (const frame of frames.slice(1)) server.send(appended(frame));
    await painted();
    reads = 0;
    fold(frames, null, true, false);
    const whole = reads;
    const FRAMES = 12;
    let spent = 0;
    for (let at = 0; at < FRAMES; at += 1) {
      const before = reads;
      server.send(appended(counter(2000 + at)));
      await painted();
      spent += reads - before;
    }
    const perFrame = spent / FRAMES;
    // A probe that recorded nothing would pass the bound below whatever the
    // column did, so the column's own reads have to be there at all.
    expect(perFrame, "the column's reads per arriving counter").toBeGreaterThan(0);
    // A twentieth of a whole fold, which the counter's own worth sits well
    // under and a single re-introduced walk of the turn - any one of the three
    // the fix deleted - does not.
    expect(perFrame, 'the counter costs the frame, not the turn').toBeLessThan(whole / 20);
  });
});

/**
 * The page's history, as a SEAM (#2354).
 *
 * `Chat.svelte` held `held.turns.flatMap((one) => one.messages)`: every message
 * the page held, rebuilt into one array. The derived was invalidated on EVERY
 * arriving frame, so the copy never waited on the page changing - only on a
 * read, an open dispatch row's timeline being the one this test drives. A copy
 * reintroduced between the page and the reader pays the whole page per frame,
 * whatever its spelling (`flatMap`, a spread, `concat`, `Array.from`), and
 * shows up here as a flattened argument. The reader's own cost is pinned
 * beside it, in `the dispatch timeline a page is walked for`.
 */
describe('the page history a turn is handed', () => {
  /** A page of `turns` turns of `size` frames each, the first opening a dispatch. */
  function page(turns: number, size: number): unknown[] {
    const rows: unknown[] = [];
    for (let at = 0; at < turns; at += 1) {
      const messages: unknown[] = [
        {
          type: 'assistant',
          uuid: `a-${at}`,
          message: {
            role: 'assistant',
            content: [
              {
                type: 'tool_use',
                id: at === 0 ? 'toolu_task' : `c${at}`,
                name: at === 0 ? 'Task' : 'Bash',
                input: at === 0 ? { description: 'review', prompt: 'read' } : { command: 'true' },
              },
            ],
          },
        },
        {
          type: 'user',
          uuid: `r-${at}`,
          message: {
            role: 'user',
            content: [
              {
                type: 'tool_result',
                tool_use_id: at === 0 ? 'toolu_task' : `c${at}`,
                content: 'ok',
              },
            ],
          },
        },
      ];
      while (messages.length < size) messages.push(counter(at * size + messages.length));
      rows.push({ key: `t${at}`, messages });
    }
    return rows;
  }

  it('hands the dispatch timeline the page turns, not a rebuilt history', async () => {
    const TURNS = 60;
    const SIZE = 20;
    const rows = page(TURNS, SIZE);

    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.send({ kind: 'page', conversation: LEAD, turns: rows, cursor: null });
    await painted();
    // A dispatch row opened the way a reader opens one: its timeline is where
    // the page history is read at all.
    subagents.sync([
      {
        name: 'review the fold',
        dispatch_id: 'toolu_task',
        agent_type: 'code-reviewer',
        running: true,
        failed: false,
        backgrounded: false,
        ended_at: null,
        calls: 1,
        tail: [],
        usage: null,
      },
    ]);
    flushSync();
    const dispatchRow = document.querySelector('details[data-sg="toolu_task"]');
    if (!(dispatchRow instanceof HTMLDetailsElement)) throw new Error('the dispatch row drew');
    // A shut row reads no history, and an arriving frame does not make it -
    // a read hoisted out of the opened branch would show up here.
    seam.seen = [];
    expect(seam.seen, 'a shut dispatch row reads no history').toEqual([]);
    server.send(appended(counter(4999)));
    await painted();
    expect(seam.seen, 'a frame arriving at a shut dispatch row reads no history').toEqual([]);
    dispatchRow.open = true;
    dispatchRow.dispatchEvent(new Event('toggle'));
    flushSync();
    expect(dispatchRow.open, 'the row whose timeline reads the history is open').toBe(true);

    const FRAMES = 8;
    for (let at = 0; at < FRAMES; at += 1) {
      server.send(appended(counter(5000 + at)));
      await painted();
    }
    // A probe that saw nothing would pass the shape checks below vacuously,
    // so the reader has to have run on the arriving frames at all.
    expect(
      seam.seen.length,
      'the timeline reader ran on every arriving frame',
    ).toBeGreaterThanOrEqual(FRAMES);
    // The page the test sent, and the arrays in it, as the identity the reader
    // must be handed: a per-turn copy (`{...turn, messages: [...]}`) is the
    // same shape and the same values, and would defeat the carry - so the
    // guard is identity, not `Array.isArray`. The arriving counters grow the
    // NEWEST turn, so its array is the store's own by design; every other
    // turn's must be the page's.
    const sent = rows as { messages: unknown[] }[];
    for (const seen of seam.seen) {
      const turns = seen as { messages?: unknown }[];
      expect(Array.isArray(seen), 'the reader is handed an array').toBe(true);
      expect(turns.length, 'the turns, not their messages').toBe(TURNS);
      expect(
        turns.slice(0, -1).every((turn, at) => turn.messages === sent[at]?.messages),
        "the page's own turns and message arrays, not copies of them",
      ).toBe(true);
    }
  });
});

/**
 * The timeline's own walk, as a COUNT (#2354's leftover).
 *
 * The flattening cut took the array copy out; the walk under it stayed. A
 * dispatch row that is open read every message of the page on every arriving
 * frame - the page is thousands of frames and a dispatch's own are tens. The
 * carry answers each turn from what it folded before, so a frame costs its own
 * worth; a miss folds that turn again, which is always correct.
 */
describe('the dispatch timeline a page is walked for', () => {
  /** A page of `turns` turns of `size` counted frames, the first opening a dispatch. */
  function page(turns: number, size: number): { messages: unknown[] }[] {
    const rows: { messages: unknown[] }[] = [];
    for (let at = 0; at < turns; at += 1) {
      const messages: unknown[] = [
        counted({
          type: 'assistant',
          uuid: `a-${at}`,
          message: {
            role: 'assistant',
            content: [
              {
                type: 'tool_use',
                id: at === 0 ? 'toolu_task' : `c${at}`,
                name: at === 0 ? 'Task' : 'Bash',
                input: at === 0 ? { description: 'review', prompt: 'read' } : { command: 'true' },
              },
            ],
          },
        }),
      ];
      while (messages.length < size) messages.push(counter(at * size + messages.length));
      rows.push({ messages });
    }
    return rows;
  }

  /**
   * The carry may read a turn it has not folded yet, and the arriving frame -
   * and nothing else.
   *
   * **The bound is the arriving turn's own size, not a share of the page.** A
   * share of the page lets the very re-walk this pins through: with twenty
   * turns on the page, one whole turn per frame stays under a twentieth of the
   * page's whole walk. A frame costs a lookup per turn and its own property
   * reads, so half a turn is orders of magnitude of room - and a re-walk of the
   * grown turn is over it.
   *
   * **Opening the row is not measured.** The first read folds the page once,
   * which is the price of showing a timeline at all; what this pins is what
   * each ARRIVING frame costs after it.
   */
  it('reads the arriving frame rather than the page', () => {
    const TURNS = 20;
    const SIZE = 200;
    const rows = page(TURNS, SIZE);
    // The row opens: the carry folds the page once.
    dispatchTimeline(rows, 'toolu_task');
    const STEPS = 12;
    let spent = 0;
    let turns = rows;
    for (let at = 0; at < STEPS; at += 1) {
      // The store's growth: the newest turn replaced, its array extended.
      const last = turns[turns.length - 1] as { messages: unknown[] };
      turns = [...turns.slice(0, -1), { messages: [...last.messages, counter(9000 + at)] }];
      reads = 0;
      dispatchTimeline(turns, 'toolu_task');
      spent += reads;
    }
    // A probe that recorded nothing would pass the bound vacuously.
    expect(spent, 'the timeline reads its frames').toBeGreaterThan(0);
    expect(spent / STEPS, 'property reads per arriving frame').toBeLessThan(SIZE / 2);
  });
});

describe('the carried fold draws what the whole fold draws', () => {
  /** One frame at a time, folded both ways: the carry may not move a row. */
  it('equals fold(), frame for frame, across a live turn', () => {
    const frames: unknown[] = [
      // A dispatched agent's frame can LEAD a turn's array: one that draws
      // nothing and finds no turn to join waits in the store and rides ahead
      // of whatever opens the next turn (`conversation.ts`'s `unturned`) -
      // and its stamp is still not the turn's.
      counted({
        type: 'system',
        subtype: 'thinking_tokens',
        uuid: 't-lead-sub',
        parent_tool_use_id: 'c0',
        estimated_tokens_delta: 5,
        timestamp: '2026-10-01T06:00:40Z',
      }),
      prompt('work'),
      // No stamp anywhere yet: the running row's span has no start until the
      // first stamped frame, whichever frame that turns out to be.
      counter(1),
      counter(2, '2026-10-01T06:00:02Z'),
      said(3),
      counter(4, '2026-10-01T06:00:20Z'),
      // Backwards: the fold's span clamps to zero, and the carry may not
      // measure from anywhere but the turn's own first stamp.
      counter(5, '2026-10-01T06:00:05Z'),
      counter(6, '2026-10-01T06:00:10Z'),
      counted({
        type: 'assistant',
        uuid: 'a-7',
        timestamp: '2026-10-01T06:00:08Z',
        message: {
          id: 'm-7',
          role: 'assistant',
          model: 'claude-opus-5',
          content: [{ type: 'tool_use', id: 'c1', name: 'Bash', input: { command: 'ls' } }],
          usage: { input_tokens: 120, output_tokens: 8 },
        },
      }),
      // A backgrounded call's task: the fold writes the fact the call's own
      // row reads, so a frame like this may never be carried.
      counted({
        type: 'system',
        subtype: 'task_started',
        task_id: 't1',
        tool_use_id: 'c1',
        is_backgrounded: true,
      }),
      counted({
        type: 'user',
        uuid: 'r1',
        message: {
          role: 'user',
          content: [{ type: 'tool_result', tool_use_id: 'c1', content: 'one file' }],
        },
      }),
      counter(8),
      counted({
        type: 'assistant',
        uuid: 'a-9',
        timestamp: '2026-10-01T06:00:09Z',
        message: {
          id: 'm-9',
          role: 'assistant',
          model: 'claude-opus-5',
          content: [{ type: 'thinking', thinking: 'weighing the next step' }],
          usage: { input_tokens: 140, output_tokens: 4 },
        },
      }),
      counter(10),
      // The task's ending, which the call's row reads as its note.
      counted({
        type: 'system',
        subtype: 'task_notification',
        task_id: 't1',
        tool_use_id: 'c1',
        status: 'completed',
        summary: 'the listing finished',
      }),
      // A dispatched agent's counter: the SUBAGENTS surface's, which the fold
      // skips whole - so the carry may not count its tokens or its stamp.
      counted({
        type: 'system',
        subtype: 'thinking_tokens',
        uuid: 't-11-sub',
        parent_tool_use_id: 'c1',
        estimated_tokens_delta: 50,
        timestamp: '2026-10-01T06:00:30Z',
      }),
      counter(12),
      counted({
        type: 'result',
        uuid: 'res1',
        duration_ms: 9000,
        duration_api_ms: 4000,
        total_cost_usd: 0.4,
        usage: {},
      }),
      counter(13),
      counter(14),
    ];
    let turn = { messages: frames.slice(0, 1) };
    for (const frame of frames.slice(1)) {
      turn = { messages: [...turn.messages, frame] };
      const whole = fold(turn.messages, null, true, false);
      expect(turnFold(turn, null, true, false), `frame ${turn.messages.length}`).toEqual(whole);
      // And the second reader of the same fold, answered from the carry.
      expect(turnFold(turn, null, true, false), `frame ${turn.messages.length} again`).toEqual(
        whole,
      );
    }
    // A page's own cut REPLACES the array rather than extending it - one frame
    // of it rewritten - and is folded rather than carried.
    const cut = { messages: [...frames.slice(0, -1), said(98), counter(99)] };
    expect(turnFold(cut, null, true, false), 'a replaced array').toEqual(
      fold(cut.messages, null, true, false),
    );
    // And the turn settling is a different fold of the same frames: the
    // running row goes and the calls the frames left open settle.
    expect(turnFold(turn, null, false, true), 'the turn settled').toEqual(
      fold(turn.messages, null, false, true),
    );
  });
});
