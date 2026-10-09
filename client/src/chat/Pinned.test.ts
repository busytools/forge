// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import Pinned from './Pinned.svelte';
import Frames from './testing/Frames.svelte';
import { git } from './git.svelte';
import type { Connection } from '../socket';
import type { TurnInfo } from './units';

/**
 * The connection the strip's segments read on mount: the browser segment
 * registers a role listener and reads the role as it draws, so this answers
 * those two and refuses nothing - these tests never press override.
 */
function untouched(): Connection {
  return {
    browserRole: () => false,
    onBrowserRole: () => () => {},
    takeBrowserRole: () => {},
  } as unknown as Connection;
}

/**
 * The strip pinned above the box: the row it is handed, and what a running
 * record draws when most of it has not arrived.
 *
 * **Which row that is belongs to the column**, not here - the running one, the
 * finished one on its beat, or none at all is decided where the turn's own row
 * has to stand aside for it, and is tested in `Chat.test.ts`. These are the
 * row's own behaviours: the fields it drops rather than fills in, and the clock
 * that keeps moving while a turn waits on a call.
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
  thinking_tokens: null,
  input_tokens: 100,
  output_tokens: 20,
  cache_read_tokens: 1000,
  cache_written_tokens: 0,
  session_cost_usd: null,
});

/** The same record before any counter has landed, which is what a turn seconds old folds to. */
const opening = (): TurnInfo => ({
  ...running(),
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_written_tokens: null,
});

let app: Record<string, unknown> | null = null;

function draw(info: TurnInfo | null): void {
  app = mount(Pinned, { target: document.body, props: { info, connection: untouched() } });
  flushSync();
}

/** The row's own markup, so an assertion reads the row and not the page. */
const row = (): string => document.querySelector('.strip')?.outerHTML ?? '';

/** What the row says, without the markup's own placeholders. */
const words = (): string => document.querySelector('.strip')?.textContent ?? '';

// The clock is a function of now, so the fixtures stand still: a frame stamped
// an hour ago would otherwise read as an hour of wait the turn never spent.
beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(AT));
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  git.sync(null);
  vi.useRealTimers();
});

describe('the strip pinned above the box', () => {
  it('draws the row of a running turn, and no figure no frame has carried', () => {
    draw(running());

    expect(row(), 'the row is pinned').not.toBe('');
    expect(row(), 'the running mark').toContain('class="ring"');
    expect(row(), 'the figures the frames carry').toContain('100\u{2191}20\u{2193}');
    expect(row(), 'and no cumulative, which only a Result carries').not.toContain('cumulative');
  });

  it('draws only what has arrived early in a turn', () => {
    // A turn seconds old: one frame, and no usage on it. Nothing is filled in
    // with a placeholder - the row grows as the frames land, and a dash on a
    // running row reads as a figure the turn reported. The strip's own
    // segments (the browser count) are not the turn's figures and are read
    // separately: what is asserted here is the row's.
    draw(opening());

    expect(row(), 'the mark and the clock').toContain('class="ring"');
    expect(words(), 'the clock').toContain('0.0s');
    expect(words(), 'and no figure no frame carried').not.toContain('\u{2191}');
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
      props: { connection: untouched() },
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

    expect(words(), 'the clock the frames did not freeze').toContain('3.0s');
  });

  it('draws nothing where the column holds no row for it and the rows hold nothing', () => {
    draw(null);

    expect(row(), 'no row and nothing to say, no strip').toBe('');
  });

  /**
   * **The strip outlives the turn.** An idle seat draws the rows it holds -
   * this used to draw nothing at all, which is what left a reader with no way
   * into a sleeping seat's tree, tasks or watchers on the page itself.
   */
  it('draws the rows on an idle seat, with no turn row above them', () => {
    git.sync({
      label: 'feat/x \u{b7} 3 files',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    draw(null);

    expect(row(), 'the strip stands without a turn').not.toBe('');
    expect(row(), 'the tree row drew').toContain('i-git');
    expect(row(), 'and no turn row was invented').not.toContain('class="ring"');
  });
});
