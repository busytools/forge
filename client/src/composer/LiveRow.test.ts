// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { processes } from '../chat/processes.svelte';
import { review } from '../chat/review.svelte';
import { tasks } from '../chat/tasks.svelte';
import { git } from '../chat/git.svelte';
import { subagents } from '../chat/subagents.svelte';
import Frames from '../chat/testing/Frames.svelte';
import Lagging from '../chat/testing/Lagging.svelte';
import type { TurnInfo } from '../chat/units';
import type { BackgroundTask } from '../session/wire';
import LiveRow from './LiveRow.svelte';

/**
 * The live row: what this seat is doing, on one line, at the top of the bar.
 *
 * **The row is the strip's successor, and it draws what the strip draws.** The
 * turn's figures and the seat's segments are the same components, mounted in
 * the same order, so what is pinned here is the row's own behaviour: the
 * fields it drops rather than fills in, the chips a store's silence keeps out,
 * the width at which a chip goes behind `+N`, and the tone a segment wearing
 * something waiting takes.
 *
 * **jsdom performs no layout**, so the overflow cases hand the row the numbers
 * a browser would have measured: `clientWidth` on the row and `offsetWidth` on
 * each chip. Everything else here reads the DOM the components actually drew.
 */

/** The instant the fixtures stand at, so a running row's clock reads its span and no wait. */
const AT = '2026-10-01T06:00:00Z';

/** A running record with the counters one frame has carried. */
const running = (): TurnInfo => ({
  running: true,
  failed: false,
  duration_ms: 0,
  api_ms: null,
  ended_at_utc: AT,
  model: 'claude-opus-5',
  thinking_tokens: 1200,
  input_tokens: 100,
  output_tokens: 20,
  cache_read_tokens: 1000,
  cache_written_tokens: 48,
  session_cost_usd: 0.42,
});

/** The same record before any counter has landed, which is what a turn seconds old folds to. */
const opening = (): TurnInfo => ({
  ...running(),
  thinking_tokens: null,
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_written_tokens: null,
  session_cost_usd: null,
});

/** One sub-agent, which is a chip on the row. */
function subagent(id: string): void {
  subagents.sync([
    {
      name: `map ${id}`,
      dispatch_id: id,
      agent_type: 'general-purpose',
      running: true,
      failed: false,
      backgrounded: false,
      ended_at: null,
      calls: 1,
      tail: [],
      usage: null,
    },
  ]);
}

/** The working tree, which is a chip on the row. */
function tree(): void {
  git.sync({
    label: 'feat/x \u{b7} 3 files',
    head: "the project's tree",
    ahead: null,
    uncommitted: null,
    pr: null,
    gate: null,
  });
}

/** One review thread, waiting to be read. */
function reviewWaiting(waiting: boolean): void {
  review.sync(
    [
      {
        id: 'r-1',
        state: waiting ? 'question' : 'resolved',
        where: 'a.rs:1',
        words: 'the answer',
        waiting,
        synthetic: false,
      },
    ],
    [],
  );
}

/** One task on the project board. */
function boardTask(): void {
  tasks.sync([
    {
      id: 't-1',
      status: 'in_progress',
      display: 'Rolling the chip',
      subject: 'Roll the chip',
      owner: 'lead',
      meta: '1 of 3',
    },
  ]);
}

/** One batch call, in flight. */
const batch = (): BackgroundTask => ({
  task_id: 'task-1',
  is_backgrounded: true,
  task_type: 'local_bash',
  description: 'run the gate',
  command: 'cargo test',
  tool_use_id: 'tu-bash',
});

/** The call's own verdict, which is what paints a settled row as failed. */
const failedCall = (): Parameters<typeof processes.sync>[3] => ({
  calls: new Map([['tu-bash', { failed: true, ended_ms: 1, stopped: false }]]),
  owners: new Map(),
  stoppedTasks: new Set(),
});

/** A batch call that settled with a failure, so its chip carries the bad tone. */
function processFailed(): void {
  processes.sync(null, [batch()], true);
  processes.sync(null, [], true, failedCall());
}

let app: Record<string, unknown> | null = null;

function draw(info: TurnInfo | null): void {
  app = mount(LiveRow, { target: document.body, props: { info } });
  flushSync();
}

/** The row's own markup, so an assertion reads the row and not the page. */
const row = (): string => document.querySelector('.live-row')?.outerHTML ?? '';

/** What the row says, without the markup's own placeholders. */
const words = (): string => document.querySelector('.live-row')?.textContent ?? '';

/** One chip's wrapper, by the name it carries. */
const chip = (name: string): HTMLElement | null =>
  document.querySelector<HTMLElement>(`[data-chip="${name}"]`);

/** The chips whose segment drew at all, in the row's order. */
function drawn(): string[] {
  return [...document.querySelectorAll<HTMLElement>('[data-chip]')]
    .filter((el) => el.firstElementChild !== null)
    .map((el) => el.dataset.chip ?? '');
}

/** A measured width on an element jsdom lays out nothing for. */
function widthOf(el: Element | null, width: number): void {
  if (el === null) return;
  Object.defineProperty(el, 'offsetWidth', { configurable: true, value: width });
}

/** The row as a browser would have measured it, then the resize that reads it. */
function measured(limit: number, widths: Record<string, number>, more: number): void {
  const rowEl = document.querySelector<HTMLElement>('.live-row');
  const moreEl = document.querySelector<HTMLElement>('.live-row .live-more');
  const track = document.querySelector<HTMLElement>('.live-row .live-segs');
  if (rowEl === null) throw new Error('the row drew nothing to measure');
  rowEl.style.padding = '0';
  rowEl.style.columnGap = '2px';
  if (track !== null) track.style.columnGap = '2px';
  Object.defineProperty(rowEl, 'clientWidth', { configurable: true, value: limit });
  for (const [name, width] of Object.entries(widths)) widthOf(chip(name), width);
  widthOf(moreEl, more);
  window.dispatchEvent(new Event('resize'));
  flushSync();
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(AT));
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  subagents.sync(null);
  processes.sync(null, null, false);
  review.sync(null, null);
  tasks.sync(null);
  git.sync(null);
  vi.useRealTimers();
});

describe('the live row', () => {
  it('draws the turn of a running record, every figure the wire carried', () => {
    draw(running());

    expect(row(), 'the row drew').not.toBe('');
    expect(row(), 'the running mark').toContain('class="ring"');
    expect(words(), 'the clock').toContain('0.0s');
    expect(words(), 'the thinking tokens').toContain('thinking 1.2k');
    expect(words(), 'in and out').toContain('100\u{2191}20\u{2193}');
    expect(words(), 'the cached share').toContain('87% cached');
    expect(words(), 'the cache write').toContain('48 written');
    expect(words(), 'the cumulative cost').toContain('$0.42 cumulative');
  });

  it('drops every figure the record has not carried, rather than drawing a zero', () => {
    draw(opening());

    expect(row(), 'the mark and the clock').toContain('class="ring"');
    expect(words(), 'the clock').toContain('0.0s');
    for (const absent of ['\u{2191}', 'cached', 'written', 'cumulative', 'thinking']) {
      expect(words(), `no ${absent} where the record carried none`).not.toContain(absent);
    }
  });

  it('moves the clock on the tick, so a row does not freeze on a call that waits', () => {
    // The fold's span only grows when a frame lands, and a turn can sit minutes
    // on one call - which is the stretch a reader is watching the clock
    // through, and the row would read one number for all of it.
    draw(opening());
    expect(words()).toContain('0.0s');

    vi.advanceTimersByTime(2000);
    flushSync();

    expect(words(), 'the clock the tick moved').toContain('2.0s');
  });

  it('keeps the clock moving while frames land faster than the tick', () => {
    // **The timer must not be re-armed per frame.** A frame is a fresh record,
    // so an effect reading the record restarts the interval before it can fire
    // - and a turn emitting about every 50 tokens emits faster than 1 s, which
    // is exactly when a reader is watching the clock. Frozen, the row reads
    // one number while the model works, then jumps when the stream pauses
    // (Ved, 2026-10-09: "it stops and then it just jumps to a bigger value").
    app = mount(Frames, {
      target: document.body,
      props: {},
    });
    flushSync();
    expect(words()).toContain('0.0s');

    // Just over three seconds of a frame every 400 ms, each flushed as the
    // stream flushes it - which is what re-runs an effect that reads the
    // record, and so what kills a timer that re-arms per frame.
    for (let at = 0; at < 8; at += 1) {
      vi.advanceTimersByTime(400);
      flushSync();
    }

    // 3.2s, not 3.0s: the 100 ms tick reads the clock as of the last tick, so
    // the display tracks the fake clock instead of lagging a whole second
    // behind it - which is the smoothness below.
    expect(words(), 'the clock the frames did not freeze').toContain('3.2s');
  });

  it('never walks the clock back when a correction arrives behind it', () => {
    // **The rule Ved asked for (live look 2026-10-11): expect locally, correct
    // when the detail lands - forward.** A record whose measured span sits
    // behind the clock the tick has already carried (a delivery lag, a fold
    // window that starts later) must not step the display backward; the clock
    // holds at its high-water mark and takes the next real reading once it
    // passes it.
    const column = mount(Lagging, {
      target: document.body,
      props: {},
    }) as unknown as { lag: () => void };
    app = column;
    flushSync();
    expect(words(), 'precondition: the clock stands at the turn start').toContain('0.0s');

    vi.advanceTimersByTime(3200);
    flushSync();
    expect(words(), 'the local tick carried it').toContain('3.2s');

    column.lag();
    flushSync();
    expect(words(), 'the late correction stepped the clock back').toContain('3.2s');

    vi.advanceTimersByTime(3000);
    flushSync();
    expect(words(), 'and the clock resumes from the reading once it passes the mark').toContain(
      '3.7s',
    );
  });

  it('draws a chip only when its store has something to state, in the fixed order', () => {
    subagent('a');
    reviewWaiting(false);
    boardTask();
    tree();
    draw(null);

    // The row's order is the strip's: subagents, git, review, tasks.
    expect(drawn()).toEqual(['subagents', 'git', 'review', 'tasks']);

    subagents.sync(null);
    review.sync(null, null);
    tasks.sync(null);
    git.sync(null);
    flushSync();
    expect(drawn(), 'the chips a store no longer states').toEqual([]);
  });

  it('takes the attention tone on the segments that hold something waiting', () => {
    reviewWaiting(true);
    subagent('a');
    draw(null);

    expect(chip('review')?.classList.contains('att'), 'the review answer, in the accent').toBe(
      true,
    );
    expect(chip('subagents')?.classList.contains('att'), 'a running agent is not waiting').toBe(
      false,
    );
  });

  it('takes the bad tone on a process that failed', () => {
    processFailed();
    draw(null);

    expect(chip('processes')?.classList.contains('bad'), 'the failed process, in red').toBe(true);
  });

  it('collapses the chips past the measured width behind +N', () => {
    subagent('a');
    processFailed();
    tree();
    reviewWaiting(false);
    boardTask();
    draw(null);
    expect(drawn()).toEqual(['subagents', 'processes', 'git', 'review', 'tasks']);

    measured(300, { subagents: 70, processes: 80, git: 90, review: 60, tasks: 50 }, 30);

    expect(
      document.querySelector('.live-row .live-more')?.textContent?.trim(),
      'two chips behind',
    ).toBe('+2');
    for (const name of ['review', 'tasks']) {
      expect(chip(name)?.classList.contains('off'), `${name} collapsed`).toBe(true);
    }
    for (const name of ['subagents', 'processes', 'git']) {
      expect(chip(name)?.classList.contains('off'), `${name} still on the row`).toBe(false);
    }
  });

  it('opens the rest of the row in a panel, in their own order, and gives the row back', () => {
    subagent('a');
    processFailed();
    tree();
    reviewWaiting(false);
    boardTask();
    draw(null);
    measured(300, { subagents: 70, processes: 80, git: 90, review: 60, tasks: 50 }, 30);

    const more = document.querySelector<HTMLButtonElement>('.live-row .live-more');
    if (more === null) throw new Error('the row drew no +N');
    more.click();
    flushSync();

    const listed = [...document.querySelectorAll<HTMLElement>('[data-panel-chip]')].map(
      (el) => el.dataset.panelChip ?? '',
    );
    expect(listed, 'the panel lists the chips the row could not hold').toEqual(['review', 'tasks']);

    more.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(document.querySelector('[data-panel-chip]'), 'the panel closed').toBeNull();
  });

  it('says what waits in the body: the ask chip in its tone, the draft chip beside it', () => {
    const onask = vi.fn();
    const ondraft = vi.fn();
    app = mount(LiveRow, {
      target: document.body,
      props: {
        info: null,
        ask: { label: 'question 1 of 2', mark: 'question', tone: 'att' },
        draft: { lines: 2 },
        onask,
        ondraft,
      },
    });
    flushSync();

    const ask = document.querySelector<HTMLElement>('.live-row .live-door.att');
    expect(ask, 'the ask drew no chip in the attention tone').not.toBeNull();
    expect(ask?.textContent, 'the ask chip says what waits').toContain('question 1 of 2');
    const draft = document.querySelector<HTMLElement>('.live-row [data-draft]');
    expect(draft?.textContent, 'the draft chip says how many lines').toContain('draft 2 lines');

    ask?.click();
    draft?.click();
    expect(onask, 'the ask chip is not a door back to the ask').toHaveBeenCalledTimes(1);
    expect(ondraft, 'the draft chip is not a door to the field').toHaveBeenCalledTimes(1);
  });

  it('gives the draft one line its own word, and stands down while the field shows', async () => {
    app = mount(LiveRow, {
      target: document.body,
      props: { info: null, ask: null, draft: { lines: 1 } },
    });
    flushSync();
    expect(
      document.querySelector('[data-draft]')?.textContent,
      'one line reads as a line',
    ).toContain('draft 1 line');

    await unmount(app);
    app = mount(LiveRow, {
      target: document.body,
      props: {
        info: null,
        ask: { label: 'post waiting', mark: 'plane', tone: 'att' },
        draft: null,
      },
    });
    flushSync();
    expect(
      document.querySelector('[data-draft]'),
      'the draft chip drew with the field shown',
    ).toBeNull();
    expect(document.querySelector('.live-row .live-door.att')).not.toBeNull();
  });

  it('draws the row as a list with a real button in every chip', () => {
    subagent('a');
    tree();
    draw(null);

    const list = document.querySelector('.live-row [role="list"]');
    expect(list, 'the chips are not a list').not.toBeNull();
    for (const item of document.querySelectorAll('[data-chip]')) {
      if (item.firstElementChild === null) continue;
      const button = item.querySelector('button');
      expect(button, 'a chip with no button').not.toBeNull();
      const name = (button?.getAttribute('aria-label') ?? button?.textContent ?? '').trim();
      expect(name, 'a chip with no readable name').not.toBe('');
    }
  });
});
