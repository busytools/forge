import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { repoPath } from '../repo';
import Notice from './Notice.svelte';
import type { NoticeSeverity } from './units';

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
const PAGE = readFileSync(repoPath('docs/book/src/ui/client/web-session.html'), 'utf8');

/** The book page's own copy of the rules it draws with. */
const BOOK = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

/** The two sheets a page reads: the app's, and the book's own drawing of it. */
const SHEETS: Array<[string, string]> = [
  ['web.css', SHEET],
  ['the book drawing', BOOK],
];

interface Rule {
  selectors: string[];
  body: string;
  /** Whether an at-rule encloses the rule, where a wide read cannot see it. */
  nested: boolean;
}

/**
 * Every style rule in a sheet, at any nesting depth.
 *
 * **A walk, not a regex.** A rule smuggled into an at-rule is what a
 * wide-width read cannot see, and a line-wise scan misses it entirely; this
 * records the nesting instead. The body of a plain rule holds declarations
 * only, so the walk steps over it whole.
 */
function rulesIn(sheet: string): Rule[] {
  const code = sheet.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: Rule[] = [];
  const walk = (from: number, to: number, nested: boolean): void => {
    let at = from;
    while (at < to) {
      const open = code.indexOf('{', at);
      if (open === -1 || open >= to) return;
      let depth = 1;
      let end = open + 1;
      while (end < to && depth > 0) {
        const char = code[end];
        if (char === '{') depth += 1;
        else if (char === '}') depth -= 1;
        end += 1;
      }
      const head = code.slice(at, open).trim();
      if (head.startsWith('@')) {
        walk(open + 1, end - 1, true);
      } else {
        found.push({
          selectors: head.split(',').map((member) => member.trim().replace(/\s+/g, ' ')),
          body: code.slice(open + 1, end - 1),
          nested,
        });
      }
      at = end;
    }
  };
  walk(0, code.length, false);
  return found;
}

/** The declarations of the first top-level rule naming `selector`, or ''. */
function topLevelBody(sheet: string, selector: string): string {
  return (
    rulesIn(sheet).find((rule) => !rule.nested && rule.selectors.includes(selector))?.body ?? ''
  );
}

describe('the chat notice row', () => {
  /**
   * **The level is the glyph.** A notice sits among the calls as a tool row,
   * and a bold level word made a second system inside the group (Ved,
   * 2026-10-30). The three shapes differ, so colour is not the only carrier on
   * screen, and the glyph's own name says the level to a screen reader.
   */
  it('draws the level as a glyph on a tool row, not as a word', () => {
    const html = render(Notice, {
      props: {
        notice: {
          severity: 'warning',
          text: 'API retry after server_error HTTP 529',
          chip: 'attempt 2 / 4',
          sub: 'retrying in 1.5s',
        },
      },
    }).body;

    expect(html, 'the row is a tool row').toContain('class="leaf nrow"');
    expect(html, 'the warning glyph leads it').toContain('#i-warning');
    expect(html, 'the glyph says the level to a screen reader').toContain('aria-label="warning"');
    expect(html, 'the warning tone colours it').toContain('gl warn');
    expect(html, 'the text is the row title').toContain('class="tn"');
    expect(html, 'the glyph and text travel together').toMatch(
      /class="lead"[\s\S]*#i-warning[\s\S]*class="tn"/,
    );
    expect(html, 'the attempt rides the row as a chip').toContain('attempt 2 / 4');
    expect(html, 'and the delay is its tail').toContain('retrying in 1.5s');
    // The glyph's aria-label carries the level, so the check is on the words a
    // reader SEES: tags stripped, the level word must not appear in any case.
    const words = html.replace(/<[^>]*>/g, ' ');
    expect(words, 'the level word is gone').not.toMatch(/warning/i);
  });

  /** Every level names its own shape; the word that carried it is gone, so
   * `info` alone would leave a reader with an unnamed image. */
  it('names every level for a screen reader', () => {
    const levels: [NoticeSeverity, string, string][] = [
      ['info', 'info', ''],
      ['warning', 'warning', ' warn'],
      ['error', 'error', ' err'],
    ];
    for (const [severity, glyph, tone] of levels) {
      const html = render(Notice, { props: { notice: { severity, text: 'a line' } } }).body;
      expect(html, `${severity} draws its shape`).toContain(`#i-${glyph}`);
      expect(html, `${severity} is an image a reader hears`).toContain('role="img"');
      expect(html, `${severity} names it`).toContain(`aria-label="${severity}"`);
      expect(html, `${severity} takes its tone`).toContain(`class="ic gl${tone}"`);
    }
  });

  /**
   * **A plain line is not a disclosure.** It has nothing to open, so a
   * chevron would promise a body, and a `summary` would make static text a
   * focus stop, a hover target and a click that changes nothing (M3).
   */
  it('draws a plain notice as a static row, not a disclosure', () => {
    const html = render(Notice, {
      props: { notice: { severity: 'info', text: 'Goal cleared' } },
    }).body;

    expect(html, 'the row still draws').toContain('Goal cleared');
    expect(html, 'with the info glyph').toContain('#i-info');
    expect(html, 'and no disclosure').not.toContain('<details');
    expect(html, 'so no summary to focus').not.toContain('<summary');
    expect(html, 'and no chevron').not.toContain('arw');
    expect(html, 'and no body').not.toContain('nbody');
  });

  /**
   * **A notice's text IS the message, and both row shapes wrap it.** The
   * plain shape is the ordinary one (`raw` is only built by `provisional()`),
   * so a pin on the provisional member alone leaves the shape readers mostly
   * meet free to take the call title's clamp - which is the C1 defect, at the
   * width it is about. The rule must also be top-level: a clamp added inside
   * an at-rule lands at the narrow widths a wide read cannot see.
   */
  it('wraps the notice text on both row shapes, in both sheets, at every width', () => {
    for (const [what, sheet] of SHEETS) {
      for (const selector of ['details.leaf.nrow > summary .tn', 'div.leaf.nrow > .rrow .tn']) {
        const body = topLevelBody(sheet, selector);
        expect(body, `${what}: ${selector} is written at the top level`).not.toBe('');
        expect(body, `${what}: ${selector} keeps line breaks`).toMatch(/white-space:\s*pre-line/);
        expect(body, `${what}: ${selector} shows all of it`).toMatch(/overflow:\s*visible/);
        expect(body, `${what}: ${selector} does not ellipsize`).toMatch(/text-overflow:\s*clip/);
        expect(body, `${what}: ${selector} breaks a long token`).toMatch(
          /overflow-wrap:\s*anywhere/,
        );
      }

      // A clamp written for the notice's title at ANY depth is the same
      // defect, so the scan hoists it out of any at-rule rather than trusting
      // a wide-width cascade read.
      const titleRules = rulesIn(sheet).filter((rule) =>
        rule.selectors.some((member) => member.includes('.nrow') && member.includes('.tn')),
      );
      expect(titleRules.length, `${what}: the scan found the title rules`).toBeGreaterThan(0);
      for (const rule of titleRules) {
        expect(
          rule.body,
          `${what}: ${rule.selectors.join(', ')} clamps the notice title`,
        ).not.toMatch(/(white-space:\s*nowrap|text-overflow:\s*ellipsis|overflow:\s*hidden)/);
      }
    }
  });

  /** The row wraps so the tails fall after a wrapped text, and the glyph's
   * group can shrink to make room instead of squeezing the words to nothing. */
  it('lays the notice row out to wrap, with the glyph group able to shrink', () => {
    for (const [what, sheet] of SHEETS) {
      for (const selector of ['details.leaf.nrow > summary', 'div.leaf.nrow > .rrow']) {
        expect(topLevelBody(sheet, selector), `${what}: ${selector} wraps`).toMatch(
          /flex-wrap:\s*wrap/,
        );
      }
      for (const selector of ['details.leaf.nrow > summary .lead', 'div.leaf.nrow > .rrow .lead']) {
        expect(topLevelBody(sheet, selector), `${what}: ${selector} can shrink`).toMatch(
          /min-width:\s*0/,
        );
      }
    }
  });

  /** The published-change URL and a long chip must not push the row past the
   * column: each tail can shrink and break (I2). */
  it('keeps the tails able to shrink and break', () => {
    for (const [what, sheet] of SHEETS) {
      for (const shape of ['details.leaf.nrow > summary', 'div.leaf.nrow > .rrow']) {
        for (const tail of ['.sub2', '.chip']) {
          const selector = `${shape} ${tail}`;
          const body = topLevelBody(sheet, selector);
          expect(body, `${what}: ${selector} is written`).not.toBe('');
          expect(body, `${what}: ${selector} shrinks`).toMatch(/flex:\s*0 1 auto/);
          expect(body, `${what}: ${selector} can go below its content`).toMatch(/min-width:\s*0/);
          expect(body, `${what}: ${selector} can break a long word`).toMatch(
            /overflow-wrap:\s*anywhere/,
          );
        }
      }
    }
  });

  /** **The warning glyph has a tone of its own**, and the state-tone check can
   * only catch a wrong token while the rule exists. */
  it('gives the warning glyph its tone', () => {
    for (const [what, sheet] of SHEETS) {
      for (const selector of ['details.leaf > summary .gl.warn', 'div.leaf > .rrow .gl.warn']) {
        expect(topLevelBody(sheet, selector), `${what}: ${selector} is written`).toMatch(
          /color:\s*var\(--warn\)/,
        );
      }
    }
  });

  it('draws a provisional notice onto its facts, marked with its own wire type', () => {
    const html = render(Notice, {
      props: {
        notice: {
          severity: 'error',
          text: 'the model is overloaded, retrying HTTP 529',
          mark: 'api_error',
          fields: [{ label: 'retry token', value: 'rt_9f3c' }],
          raw: '{"type":"system","subtype":"api_error"}',
        },
      },
    }).body;

    expect(html, 'a provisional row is a disclosure').toContain('<details class="leaf nrow"');
    expect(html, 'the error glyph leads it').toContain('#i-error');
    expect(html, 'the wire subtype rides the summary as a chip').toContain('api_error');
    expect(html, 'the row carries a chevron because it opens').toContain('arw');
    expect(html, 'and a value only the facts carry proves the body drew').toContain('rt_9f3c');
  });

  /** The banner went with the markup that drew it; a rule left behind is a
   * third surface picking the name up again. The control proves the scanner
   * reads the conversation's rules at all, so an empty result means absence
   * rather than a blind matcher. */
  it('leaves the banner chrome out of the sheet', () => {
    for (const [what, sheet] of SHEETS) {
      const members = rulesIn(sheet).flatMap((rule) => rule.selectors);
      const conversation = members.filter((selector) => selector.includes('.conv'));
      expect(conversation.length, `${what}: the scanner reads the conversation`).toBeGreaterThan(0);
      expect(
        conversation.filter((selector) => selector.includes('.notice')),
        `${what}: a banner rule survived the markup it drew`,
      ).toEqual([]);
    }
  });

  /** A bare `.notice` rule reaches the composer's own notice row, which wears
   * the name for its own chrome. The chat's rules are not the only ones that
   * can collide, so the guard is on the bare name, not on this PR's scope. */
  it('wears no notice name bare enough to reach another surface', () => {
    for (const [what, sheet] of SHEETS) {
      const bare = rulesIn(sheet)
        .flatMap((rule) => rule.selectors)
        .filter((selector) => (selector.split(/\s+|[>+~]/)[0] ?? '').includes('.notice'));
      expect(bare, `${what}: an unscoped .notice rule reaches another surface`).toEqual([]);
    }
  });
});
