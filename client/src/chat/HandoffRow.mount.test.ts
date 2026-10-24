// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import HandoffRow from './HandoffRow.svelte';
import type { HandoffRecord } from '../wire/handoff';

/**
 * The row's own open, as MOUNTED DOM.
 *
 * **The summary's chip is the only account a closed row gives**, so the two
 * kinds of trigger are pinned here: a fill row says the share, and a compact
 * backstop says its reason rather than a percentage that did not fire it.
 */
const a_handoff = (overrides: Partial<HandoffRecord> = {}): HandoffRecord => ({
  from: '9cfe315c-1111-2222-3333-444444444444',
  to: '4b21e0a7-5555-6666-7777-888888888888',
  reason: 'fill',
  fill_percent: 85,
  total_tokens: 850_000,
  max_tokens: 1_000_000,
  compaction_count: 0,
  at_percent: 85,
  ground_truth: [{ label: 'seat', lines: ['Busytools/forge/lead'] }],
  judgment: [{ label: 'Next', lines: ['the client row'] }],
  ...overrides,
});

describe('the handoff row, mounted', () => {
  it('wears the fill for a fill trigger and the reason for a backstop', () => {
    const filled = mount(HandoffRow, {
      target: document.body,
      props: { handoff: a_handoff() },
    });
    try {
      flushSync();
      expect(document.querySelector('summary')?.textContent ?? '').toContain('85%');
    } finally {
      void unmount(filled);
      document.body.innerHTML = '';
    }

    const backstop = mount(HandoffRow, {
      target: document.body,
      props: { handoff: a_handoff({ reason: 'compaction' }) },
    });
    try {
      flushSync();
      const summary = document.querySelector('summary')?.textContent ?? '';
      expect(summary, 'a backstop says why it fired').toContain('a compaction came first');
      expect(summary, 'and not a window share it did not fire on').not.toContain('85%');
    } finally {
      void unmount(backstop);
      document.body.innerHTML = '';
    }
  });

  it('opens onto both halves of the brief, and holds nothing while closed', () => {
    const app = mount(HandoffRow, {
      target: document.body,
      props: { handoff: a_handoff() },
    });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'closed, the brief is not drawn').not.toContain(
        'the client row',
      );

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      const body = document.querySelector('.hbody')?.textContent ?? '';
      expect(body, 'PART 1 draws').toContain('PART 1');
      expect(body, 'and its facts with it').toContain('Busytools/forge/lead');
      expect(body, 'PART 2 draws').toContain('PART 2');
      expect(body, 'and the judgment with it').toContain('the client row');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
