// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Notice from './Notice.svelte';
import type { Notice as NoticeRow } from './units';
import { violationsOf } from '../testing/axe';

/**
 * The provisional notice, as MOUNTED DOM.
 *
 * **The first cut drew a wall of JSON and a stray line.** A reader looking at
 * it found an empty row under every notice - the chevron, a block-level icon,
 * dropping onto a line of its own - and an open body that was the raw frame
 * rather than the frame's facts. The facts then landed on a shared
 * label/value row that pushed each label and its value to opposite ends of
 * the box, which reads as two unrelated ends rather than one line (Ved,
 * 2026-10-28). So the summary keeps the expander inside it, the body draws
 * each fact as one `label: value` line, and the frame itself waits behind
 * its own control (rule 25 keeps it reachable).
 */
const a_row = (overrides: Partial<NoticeRow> = {}): NoticeRow => ({
  severity: 'error',
  text: 'Bash refused: Contains simple_expansion',
  mark: 'permission_denied',
  fields: [
    { label: 'tool', value: 'Bash' },
    { label: 'call', value: 'toolu_denied' },
  ],
  raw: '{"type":"system","subtype":"permission_denied","session_id":"s1"}',
  ...overrides,
});

/** The sheet, because the extra row was a LAYOUT fault a DOM read cannot see.
 * Read by path rather than `import.meta.url`: the jsdom environment does not
 * hand back a file URL. */
const SHEET = readFileSync('src/assets/web.css', 'utf8');

describe('the provisional notice, mounted', () => {
  it('keeps the expander on the header row and the frame behind its own control', () => {
    const app = mount(Notice, { target: document.body, props: { notice: a_row() } });
    try {
      flushSync();
      const row = document.querySelector('details.notice');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      expect(row.open, 'a provisional row starts closed').toBe(false);
      expect(
        row.querySelector('summary .arw'),
        'the chevron sits inside the header row, not on a line under it',
      ).not.toBeNull();
      expect(
        row.querySelector('.nbody pre'),
        'the raw frame is not drawn until its own control is open',
      ).toBeNull();

      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();

      const body = row.querySelector('.nbody');
      expect(body, 'the open row carries a body').not.toBeNull();
      expect(
        body?.querySelectorAll('.nfact').length,
        'each fact draws as one line, label and value together',
      ).toBe(2);
      expect(body?.textContent, 'the body is the facts, not the payload').not.toContain(
        '"session_id"',
      );

      const raw = row.querySelector('details.raw');
      if (!(raw instanceof HTMLDetailsElement)) {
        throw new Error('the frame has no control of its own');
      }
      expect(row.querySelector('details.raw pre'), 'and waits for that control').toBeNull();

      raw.open = true;
      raw.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(
        row.querySelector('details.raw pre')?.textContent,
        'the frame itself is still reachable',
      ).toContain('"session_id"');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  /**
   * **A row whose facts share a label is the ordinary case, not an edge.**
   * The memory-recall row lists one `path` fact per memory, and the original
   * keyed-each used `field.label` alone - two paths gave Svelte two equal
   * keys, and the row threw as it drew. The fold and the server render both
   * pass; only a mount sees it.
   */
  it('draws two memories with the same fact label, one line each', () => {
    const app = mount(Notice, {
      target: document.body,
      props: {
        notice: a_row({
          severity: 'info',
          mark: 'memory_recall',
          text: 'Recalled 2 memories',
          fields: [
            { label: 'path', value: '/home/ved/.claude/memory/one.md' },
            { label: 'path', value: '/home/ved/.claude/memory/two.md' },
          ],
        }),
      },
    });
    try {
      flushSync();
      const row = document.querySelector('details.notice');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();

      expect(row.querySelectorAll('.nfact').length, 'each memory draws its own fact line').toBe(2);
      expect(row.textContent, 'the first path is drawn').toContain(
        '/home/ved/.claude/memory/one.md',
      );
      expect(row.textContent, 'and the second beside it').toContain(
        '/home/ved/.claude/memory/two.md',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  /**
   * The layout assertion the DOM cannot make: `summary` laid out as a row is
   * what puts the expander on the header line. A change that drops it brings
   * the empty extra row back under every notice.
   */
  it('lays the header out in one row, which is what keeps the expander there', () => {
    const rule = /\.conv details\.notice > summary \{([^}]*)\}/.exec(SHEET)?.[1] ?? '';
    expect(rule, 'the sheet has no rule for the provisional summary').not.toBe('');
    expect(rule, 'a block icon drops the chevron onto a line of its own').toMatch(
      /display:\s*flex/,
    );
  });

  /**
   * The fact line's own pin: the shared label/value row spreads the two ends
   * apart, and reusing it here is what made `tool` and `Bash` read as two
   * unrelated sides of the box (Ved, 2026-10-28).
   */
  it('keeps each fact line together rather than pushing its two ends apart', () => {
    const rule = /\.conv details\.notice \.nfact \{([^}]*)\}/.exec(SHEET)?.[1];
    expect(rule, 'the sheet has no rule for the provisional fact line').toBeDefined();
    expect(rule ?? '', 'a justified pair splits a fact into two ends').not.toMatch(/space-between/);
  });

  /** The open body under axe: a closed `<details>` is walked by nothing. */
  it('draws the open row with no violations', async () => {
    const main = document.createElement('main');
    document.body.append(main);
    const app = mount(Notice, { target: main, props: { notice: a_row() } });
    try {
      flushSync();
      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(main.textContent ?? '', 'the row drew, so axe saw it').toContain('toolu_denied');
      expect(await violationsOf(`<main>${main.innerHTML}</main>`)).toEqual([]);
    } finally {
      void unmount(app);
      main.remove();
      document.body.innerHTML = '';
    }
  });
});
