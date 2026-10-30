// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Notice from './Notice.svelte';
import type { Notice as NoticeRow } from './units';
import { repoPath } from '../repo';
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
 * 2026-10-28). So the row is a tool row, the summary keeps the expander inside
 * it, the body draws each fact as one `label: value` line, and the frame
 * itself waits behind its own control (rule 25 keeps it reachable).
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

/** A plain line: the shape ordinary notices use, since only a provisional
 * frame carries `raw`. */
const a_plain = (overrides: Partial<NoticeRow> = {}): NoticeRow => ({
  severity: 'info',
  text: 'Goal cleared',
  ...overrides,
});

/** The sheet, because the extra row was a LAYOUT fault a DOM read cannot see.
 * Read by path rather than `import.meta.url`: the jsdom environment does not
 * hand back a file URL. */
const SHEET = readFileSync('src/assets/web.css', 'utf8');
const PAGE = readFileSync(repoPath('docs/book/src/ui/client/web-session.html'), 'utf8');

/** The book page's own copy of the rules it draws with. */
const BOOK = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

/** The two sheets a page reads: the app's, and the book's own drawing of it. */
const SHEETS: Array<[string, string]> = [
  ['web.css', SHEET],
  ['the book drawing', BOOK],
];

/**
 * What the cascade sets on a notice's title, for one row shape and one sheet.
 *
 * **jsdom evaluates no media features**, so this reads the wide-width cascade;
 * the sheet scan in `Notice.test.ts` is what catches a clamp an at-rule
 * carries. Together they cover both directions of the C1 defect.
 */
function titleStyle(sheet: string, notice: NoticeRow): Record<string, string> {
  // Mounted rather than server-rendered: this file runs in jsdom, where the
  // component resolves to its client build and `svelte/server` cannot draw it.
  const host = document.createElement('div');
  const app = mount(Notice, { target: host, props: { notice } });
  flushSync();
  const markup = host.innerHTML;
  void unmount(app);
  const dom = new JSDOM(
    `<!doctype html><html><head><style>${sheet}</style></head><body><div class="conv">${markup}</div></body></html>`,
    { pretendToBeVisual: true },
  );
  const title = dom.window.document.querySelector('.tn');
  if (title === null) throw new Error('the row drew no title');
  const style = dom.window.getComputedStyle(title);
  return {
    'white-space': style.getPropertyValue('white-space'),
    overflow: style.getPropertyValue('overflow'),
    'text-overflow': style.getPropertyValue('text-overflow'),
  };
}

/** The declarations of the first rule whose selector list names `selector` -
 * a list, because the notice row shares the leaf row's rule. Matching a whole
 * member matters: `details.leaf > summary` is a prefix of the glyph rule. */
function ruleBody(selector: string): string {
  const code = SHEET.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const rule of code.matchAll(/([^{}]+)\{([^}]*)\}/g)) {
    const members = (rule[1] ?? '').split(',').map((member) => member.trim().replace(/\s+/g, ' '));
    if (members.includes(selector)) return rule[2] ?? '';
  }
  return '';
}

describe('the provisional notice, mounted', () => {
  it('keeps the expander on the header row and the frame behind its own control', () => {
    const app = mount(Notice, { target: document.body, props: { notice: a_row() } });
    try {
      flushSync();
      const row = document.querySelector('details.leaf');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      expect(row.matches('details.leaf.nrow'), 'the notice draws as a tool row').toBe(true);
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
      const row = document.querySelector('details.leaf');
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
   * The layout assertion the DOM cannot make: the row is the same `summary`
   * every call row uses, laid out as a flex row - which is what puts the
   * expander on the header line. A notice that grew its own summary rule would
   * be a second row keeping the chrome in step by hand.
   */
  it("lays the header out with the leaf row's own rule", () => {
    const rule = ruleBody('details.leaf > summary');
    expect(rule, 'the sheet has no rule for a leaf row summary').not.toBe('');
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
    const rule = /details\.leaf\.nrow \.nfact \{([^}]*)\}/.exec(SHEET)?.[1];
    expect(rule, 'the sheet has no rule for the fact line').toBeDefined();
    expect(rule ?? '', 'a justified pair splits a fact into two ends').not.toMatch(
      /justify-content/,
    );
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

  /**
   * **Both row shapes wrap, proved where the cascade lands.** The rule pin in
   * `Notice.test.ts` reads the sheet; this reads the computed title, so
   * deleting a shape's selector member (which leaves that shape on the base
   * clamp) fails here even when the other member keeps the text rule alive.
   */
  it('wraps the title on both row shapes, in both sheets', () => {
    const shapes: Array<[string, NoticeRow]> = [
      ['plain', a_plain({ severity: 'warning', text: 'a long line that must be read' })],
      ['provisional', a_row({ severity: 'warning', text: 'a long line that must be read' })],
    ];
    for (const [shape, notice] of shapes) {
      for (const [what, sheet] of SHEETS) {
        const style = titleStyle(sheet, notice);
        expect(style['white-space'], `${shape}/${what} keeps line breaks`).toBe('pre-line');
        expect(style.overflow, `${shape}/${what} shows all of it`).toBe('visible');
        expect(style['text-overflow'], `${shape}/${what} does not ellipsize`).toBe('clip');
      }
    }
  });

  /**
   * **The static row under axe.** It is a div, not a disclosure, so nothing
   * else walks it: all three levels, mounted as a reader meets them.
   */
  it('draws every static level with no violations', async () => {
    const main = document.createElement('main');
    document.body.append(main);
    const levels: NoticeRow[] = [
      a_plain({ severity: 'info', text: 'Goal cleared' }),
      a_plain({ severity: 'warning', text: 'Near rate-limit threshold.' }),
      a_plain({ severity: 'error', text: 'The tool call was refused.' }),
    ];
    const apps = levels.map((notice) => mount(Notice, { target: main, props: { notice } }));
    try {
      flushSync();
      expect(main.textContent ?? '', 'all three levels drew, so axe saw them').toContain(
        'The tool call was refused.',
      );
      expect(await violationsOf(`<main>${main.innerHTML}</main>`)).toEqual([]);
    } finally {
      for (const app of apps) void unmount(app);
      main.remove();
      document.body.innerHTML = '';
    }
  });
});
