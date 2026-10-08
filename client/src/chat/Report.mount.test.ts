// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Report from './Report.svelte';
import type { TurnInfo } from './units';

/**
 * The report's own open, as MOUNTED DOM.
 *
 * **A closed report carries its strip and nothing else, and only a mount can
 * show the reader's own open**: SSR pins neither `bind:open` nor the prop's
 * default, so the row is mounted closed (the production default) and then
 * opened the way a reader opens it.
 */
const FULL: TurnInfo = {
  running: false,
  failed: false,
  duration_ms: 161_000,
  api_ms: 64_000,
  ended_at_utc: '2026-09-29T10:18:31.000Z',
  model: 'claude-opus-5',
  thinking_tokens: 434,
  input_tokens: 4_231,
  output_tokens: 1_102,
  cache_read_tokens: 108_442,
  cache_written_tokens: 3_180,
  session_cost_usd: 4.82,
};

describe('the report row, mounted', () => {
  it("draws its facts onto the reader's open, and holds nothing while closed", () => {
    const app = mount(Report, { target: document.body, props: { info: FULL } });
    try {
      flushSync();
      expect(document.querySelector('.tibody'), 'closed, no facts grid').toBeNull();

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.querySelector('.tibody')?.textContent ?? '', 'the facts draw').toContain(
        'claude-opus-5',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
