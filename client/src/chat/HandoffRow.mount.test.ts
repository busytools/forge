// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it, vi } from 'vitest';

import HandoffRow from './HandoffRow.svelte';
import type { HandoffRecord } from '../wire/handoff';
import { violationsOf } from '../testing/axe';

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
  at_percent: 85,
  at: '2026-10-24T08:00:00Z',
  cycle: { requests: 514, turns: 96, prompt_tokens: 4_970_000_000 },
  successor: { turns: 10, reread_files: 3, questions: 1, corrections: 0 },
  ground_truth: [{ label: 'seat', lines: ['Busytools/forge/lead'] }],
  judgment: [{ label: 'Next', lines: ['the client row'] }],
  judgment_turn: [],
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
      expect(body, 'the full previous id is shown').toContain(
        '9cfe315c-1111-2222-3333-444444444444',
      );
      expect(body, 'and the successor id').toContain('4b21e0a7-5555-6666-7777-888888888888');
      expect(body, 'the recovery command is named').toContain('/resume');
      expect(
        document.querySelectorAll('.hbody button.cp').length,
        'each id carries the control that copies it',
      ).toBe(2);
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  /**
   * The record's own numbers, which the row used to narrow and never draw.
   * They sit with the brief's facts, so the tuning numbers the handoff fires
   * for are readable on the row that recorded the swap.
   */
  it('draws the cycle, the successor and the time under the meta', () => {
    const app = mount(HandoffRow, {
      target: document.body,
      props: { handoff: a_handoff() },
    });
    try {
      flushSync();
      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      const body = document.querySelector('.hbody')?.textContent ?? '';
      expect(body, 'the record states when the swap happened').toContain('2026-10-24T08:00:00Z');
      expect(body, 'the cycle it ended, in its own counts').toContain('514 requests');
      expect(body, 'and what the cycle spent').toContain('4,970,000,000 prompt tokens');
      expect(body, 'then how the successor went').toContain('3 rereads');
      expect(body, 'which is measured in its own words').toContain('1 question');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  /** An unmeasured successor is absent until its first turns are read: a row
   * of zeros would read as the best possible handover. */
  it('writes no successor line for one that has not been measured', () => {
    const app = mount(HandoffRow, {
      target: document.body,
      props: { handoff: a_handoff({ successor: null }) },
    });
    try {
      flushSync();
      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      const body = document.querySelector('.hbody')?.textContent ?? '';
      expect(body, 'the row still says when').toContain('2026-10-24T08:00:00Z');
      expect(body, 'but nothing of a successor nobody measured').not.toContain('rereads');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  /**
   * The row's OPEN body under axe, which no server render reaches: axe walks
   * nothing inside a closed `<details>`, so the brief's own markup is audited
   * in the state a reader opens it into - and inside the landmark its page
   * gives it, because rendered alone every row reports the page-level
   * `region` rule rather than anything about itself.
   */
  it('draws the open brief with no violations', async () => {
    const main = document.createElement('main');
    document.body.append(main);
    const app = mount(HandoffRow, { target: main, props: { handoff: a_handoff() } });
    try {
      flushSync();
      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(main.textContent ?? '', 'the row drew, so axe saw it').toContain('PART 1');
      expect(await violationsOf(`<main>${main.innerHTML}</main>`)).toEqual([]);
    } finally {
      void unmount(app);
      main.remove();
      document.body.innerHTML = '';
    }
  });

  it('copies the previous full id, which is what /resume needs', async () => {
    const writes: string[] = [];
    vi.stubGlobal('navigator', {
      clipboard: {
        writeText: (text: string) => {
          writes.push(text);
          return Promise.resolve();
        },
      },
    });
    const app = mount(HandoffRow, {
      target: document.body,
      props: { handoff: a_handoff() },
    });
    try {
      flushSync();
      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      const first = document.querySelector('.hbody button.cp');
      if (!(first instanceof HTMLButtonElement)) throw new Error('the row drew no copy control');
      first.click();
      await Promise.resolve();
      expect(writes, 'the whole previous id lands on the clipboard').toEqual([
        '9cfe315c-1111-2222-3333-444444444444',
      ]);
    } finally {
      vi.unstubAllGlobals();
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
