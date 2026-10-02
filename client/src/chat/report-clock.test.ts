// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Report from './Report.svelte';
import type { TurnInfo } from './units';

/**
 * The clock in a running row's body, which is the row's own `elapsed` drawn
 * again as a fact of the body.
 *
 * **Its own file because it is the one thing here that needs an effect**: the
 * tick that keeps the figure moving is an interval, and `Report.test.ts`
 * renders through the server renderer, which runs no effect at all - and the
 * `$effect` a client build then meets there is an orphan. Everything the row
 * draws without a tick is asserted there, beside the rest of the settled row.
 */

const AT = '2026-10-01T06:00:00.000Z';

/** A running record, with the clock at the instant of its own last frame. */
const running: TurnInfo = {
  running: true,
  failed: false,
  duration_ms: 0,
  api_ms: null,
  ended_at_utc: AT,
  model: 'claude-opus-5',
  thinking_tokens: 434,
  input_tokens: 4_231,
  output_tokens: 1_102,
  cache_read_tokens: 108_442,
  cache_written_tokens: 3_180,
  session_cost_usd: null,
};

let app: Record<string, unknown> | null = null;

/** The `elapsed` fact the body draws, which is the figure the row leads with. */
function body(): string {
  const fact = [...document.querySelectorAll('.tibody .fact')].find(
    (one) => one.querySelector('b')?.textContent === 'elapsed',
  );
  return fact?.querySelector('span')?.textContent ?? '';
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.useRealTimers();
});

describe('the clock a running row draws in its body', () => {
  it('moves on the tick, so the body does not freeze on a call that waits', () => {
    // The fold's span only grows when a frame lands, and a turn can sit minutes
    // on one call: a body that read the clock once would hold one number for
    // the whole of it, which is the stretch a reader is watching it through.
    vi.useFakeTimers();
    vi.setSystemTime(new Date(AT));
    app = mount(Report, { target: document.body, props: { info: running } });
    flushSync();

    expect(body(), 'the span the record carries at its last frame').toBe('0.0s');

    vi.advanceTimersByTime(5000);
    flushSync();

    expect(body(), 'the clock the tick moved').toBe('5.0s');
  });
});
