import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Notice from './Notice.svelte';
import type { NoticeSeverity } from './units';

/**
 * The sheet, as text: what chrome a row draws is a class question, and jsdom
 * does no layout, so a rendered assertion cannot see it.
 */
const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** The declarations of the first rule whose selector list names `selector`. */
function ruleBody(selector: string): string {
  const code = SHEET.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const rule of code.matchAll(/([^{}]+)\{([^}]*)\}/g)) {
    const members = (rule[1] ?? '').split(',').map((member) => member.trim().replace(/\s+/g, ' '));
    if (members.includes(selector)) return rule[2] ?? '';
  }
  return '';
}

/** Every selector member the sheet writes that contains `fragment`. */
function selectorsMatching(fragment: string): string[] {
  const code = SHEET.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: string[] = [];
  for (const rule of code.matchAll(/([^{}]+)\{/g)) {
    for (const member of (rule[1] ?? '').split(',')) {
      if (member.trim().includes(fragment)) found.push(member.trim());
    }
  }
  return found;
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
    expect(html, 'the level word is gone').not.toContain('Warning');
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
   * A notice's text IS the message: a narrow row must wrap it rather than
   * clamp it the way a call's title clamps (C1). The plain row covers the
   * same rule because both row shapes share it.
   */
  it('wraps the notice text where a call title clamps', () => {
    const rule = ruleBody('details.leaf.nrow > summary .tn');
    expect(rule, 'the sheet has no rule for the notice title').not.toBe('');
    expect(rule, 'keeps the line breaks the text arrived with').toMatch(/white-space:\s*pre-line/);
    expect(rule, 'and shows all of it').toMatch(/overflow:\s*visible/);
    expect(rule, 'rather than ellipsizing it').toMatch(/text-overflow:\s*clip/);
    expect(rule, 'which a clamp would undo').not.toMatch(/nowrap/);
  });

  /** The published-change URL and a long chip must not push the row past the
   * column: each tail can shrink and break (I2). */
  it('keeps the tails able to shrink and break', () => {
    for (const selector of [
      'details.leaf.nrow > summary .sub2',
      'details.leaf.nrow > summary .chip',
    ]) {
      const rule = ruleBody(selector);
      expect(rule, `${selector} is written`).not.toBe('');
      expect(rule, `${selector} shrinks`).toMatch(/flex:\s*0 1 auto/);
      expect(rule, `${selector} can go below its content`).toMatch(/min-width:\s*0/);
      expect(rule, `${selector} can break a long word`).toMatch(/overflow-wrap:\s*anywhere/);
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
    const conversation = selectorsMatching('.conv ');
    expect(conversation.length, 'the scanner reads the conversation').toBeGreaterThan(0);
    const banner = conversation.filter((selector) => selector.includes('.notice'));
    expect(banner, 'a banner rule survived the markup it drew').toEqual([]);
  });
});
