import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Notice from './Notice.svelte';

/**
 * The sheet, as text: what chrome a row draws is a class question, and jsdom
 * does no layout, so a rendered assertion cannot see it.
 */
const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** Every rule body the sheet writes for this exact selector. */
function rulesFor(selector: string): string[] {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return [...SHEET.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))].map(
    (match) => match[1] ?? '',
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
    expect(html, 'the attempt rides the row as a chip').toContain('attempt 2 / 4');
    expect(html, 'and the delay is its tail').toContain('retrying in 1.5s');
    expect(html, 'the level word is gone').not.toContain('Warning');
  });

  it('draws a plain notice with nothing to open and no chevron', () => {
    const html = render(Notice, {
      props: { notice: { severity: 'info', text: 'Goal cleared' } },
    }).body;

    expect(html, 'the row still draws').toContain('Goal cleared');
    expect(html, 'with the info glyph').toContain('#i-info');
    expect(html, 'and nothing to open').not.toContain('arw');
    expect(html, 'so no empty body under it').not.toContain('nbody');
  });

  it('draws a provisional notice onto its facts, marked with its own wire type', () => {
    const html = render(Notice, {
      props: {
        notice: {
          severity: 'error',
          text: 'API request failed HTTP 529',
          mark: 'api_error',
          fields: [{ label: 'status', value: 'HTTP 529' }],
          raw: '{"type":"system","subtype":"api_error"}',
        },
      },
    }).body;

    expect(html, 'the error glyph leads it').toContain('#i-error');
    expect(html, 'the wire subtype rides the summary as a chip').toContain('api_error');
    expect(html, 'the row carries a chevron because it opens').toContain('arw');
    expect(html, 'and the frame facts sit behind it').toContain('HTTP 529');
  });

  /** The banner went with the markup that drew it; a rule left behind is a
   * third surface picking the name up again. */
  it('leaves the banner chrome out of the sheet', () => {
    for (const selector of [
      '.conv .notice',
      '.conv .notice.info',
      '.conv details.notice > summary',
    ]) {
      expect(rulesFor(selector).length, `${selector} is still written`).toBe(0);
    }
  });
});
