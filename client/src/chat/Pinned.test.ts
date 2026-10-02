// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { boxed } from '../session/testing/props.svelte';
import type { Turn as HeldTurn } from './conversation';
import Pinned from './Pinned.svelte';

/**
 * The strip pinned above the box: the newest turn's metadata row while it runs,
 * and the finished row's own beat before it detaches into the turn.
 *
 * Read off the document rather than through an SSR render, because all of that
 * is one state machine over effects and a timer, and an SSR render runs
 * neither.
 */

/** The instant the fixtures stand at, so a running row's clock reads its span and no wait. */
const AT = '2026-10-01T06:00:00Z';

/** One assistant message, with the counters its own call carried. */
const said = (at: string, input = 100): unknown => ({
  type: 'assistant',
  uuid: `a-${at}`,
  timestamp: at,
  message: {
    id: `m-${at}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text: 'working' }],
    usage: {
      input_tokens: input,
      output_tokens: 20,
      cache_read_input_tokens: 1000,
      cache_creation_input_tokens: 0,
    },
  },
});

/** The frame a turn ends on, which is the only carrier of a session cost. */
const ended = (): unknown => ({
  type: 'result',
  uuid: 'r1',
  duration_ms: 42_000,
  duration_api_ms: 20_000,
  total_cost_usd: 12.5,
  usage: {
    input_tokens: 100,
    output_tokens: 20,
    cache_read_input_tokens: 1000,
    cache_creation_input_tokens: 5,
  },
});

/** A turn the frames are still writing. */
const running = (key = 't1', input = 100): HeldTurn => ({
  key,
  messages: [said(AT, input)],
  live: true,
});

/** The same turn as a page settled it, carrying the frame that ended it. */
const settled = (key = 't1'): HeldTurn => ({
  key,
  messages: [said(AT), ended()],
  live: false,
});

let app: Record<string, unknown> | null = null;

/** A mounted pin over props a test can move, which is how a turn arrives and ends. */
function draw(initial: Record<string, unknown>): Record<string, unknown> {
  const page = boxed({ turn: null, cwd: null, slot: null, ...initial });
  app = mount(Pinned, { target: document.body, props: page });
  flushSync();
  return page;
}

/** The pinned row's own markup, so an assertion reads the row and not the page. */
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
  vi.useRealTimers();
});

describe('the strip pinned above the box', () => {
  it('draws the row of a running turn, and no figure no frame has carried', () => {
    draw({ turn: running() });

    expect(row(), 'the row is pinned').not.toBe('');
    expect(row(), 'the running mark').toContain('class="ring"');
    expect(row(), 'the figures the frames carry').toContain('100\u{2191}20\u{2193}');
    expect(row(), 'and no cumulative, which only a Result carries').not.toContain('cumulative');
  });

  it('draws only what has arrived early in a turn', () => {
    // A turn seconds old: one frame, and no usage on it. Nothing is filled in
    // with a placeholder - the row grows as the frames land, and a dash on a
    // running row reads as a figure the turn reported.
    draw({
      turn: {
        key: 't1',
        live: true,
        messages: [
          {
            type: 'assistant',
            uuid: 'a1',
            timestamp: '2026-10-01T06:00:00Z',
            message: {
              id: 'm1',
              role: 'assistant',
              model: 'claude-opus-5',
              content: [{ type: 'text', text: 'working' }],
            },
          },
        ],
      },
    });

    expect(row(), 'the mark and the clock').toContain('class="ring"');
    expect(words().trim(), 'the clock, and nothing else').toBe('0.0s');
    expect(row(), 'and no dash standing in for a figure').not.toContain('>-<');
  });

  it('draws nothing for a conversation whose newest turn has settled', () => {
    draw({ turn: settled() });

    expect(row(), 'the finished row belongs to the turn it is drawn in').toBe('');
  });

  it('holds the finished row for a beat before it detaches', () => {
    vi.useFakeTimers();
    const page = draw({ turn: running() });
    expect(row()).toContain('class="ring"');

    page.turn = settled();
    flushSync();

    expect(row(), 'the finish mark rather than the ring').toContain('i-check');
    expect(row(), 'the span the Result states').toContain('42.0s');
    expect(row(), 'and the cumulative, which only a Result carries').toContain('$12.50 cumulative');

    vi.advanceTimersByTime(450);
    flushSync();

    expect(row(), 'the row is gone once the turn owns its strip again').toBe('');
  });

  it('never pins a row for a turn it did not watch run', () => {
    // The newest row can be replaced under this pin by a key the frames never
    // built - a page for another occupant, a row the reader never saw open -
    // and a beat still in flight must not hand that turn's finished row to the
    // pin: the settled row belongs to the turn it is drawn in.
    vi.useFakeTimers();
    const page = draw({ turn: running() });

    page.turn = settled('t1');
    flushSync();
    expect(row(), 'the beat is on').toContain('cumulative');

    page.turn = settled('t2');
    flushSync();

    expect(row(), 'and the replacement turn draws nothing here').toBe('');
  });

  it('takes the pin back for the next turn inside the beat of the one before', () => {
    vi.useFakeTimers();
    const page = draw({ turn: running() });

    page.turn = settled();
    flushSync();
    expect(row()).toContain('cumulative');

    page.turn = running('t2', 500);
    flushSync();

    expect(row(), 'the next turn draws its own figures').toContain('500\u{2191}');
    expect(row(), 'and its own running mark').toContain('class="ring"');

    vi.advanceTimersByTime(450);
    flushSync();

    expect(row(), 'the beat that belonged to the turn before does not clear it').not.toBe('');
  });
});
