import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Inbound from './Inbound.svelte';
import type { InboundLeaf } from './units';

/**
 * The inbound delivery row, as markup and as rules.
 *
 * The row's own classes and its body's are read the way `Hook.test.ts` reads
 * its row's; what is pinned here is the pair of facts this row was fixed for:
 * a tail that only repeats the title is not drawn, and the tail takes the
 * width the title leaves rather than taking the title's letters.
 */

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
const PAGE = readFileSync(
  new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/** The book page's own copy of the rules it draws with. */
const BOOK = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

const row = (extra: Partial<InboundLeaf> = {}): InboundLeaf => ({
  key: 'delivery-1',
  title: 'the morning sweep',
  body: 'the morning sweep',
  elevated: false,
  ...extra,
});

const draw = (over: Partial<InboundLeaf> = {}): string =>
  render(Inbound, { props: { row: row(over) } }).body;

/** The closed row's own words, with Svelte's block markers and the tags off. */
const summaryWords = (body: string): string =>
  body
    .slice(body.indexOf('<summary'), body.indexOf('</summary>'))
    .replace(/<!--.*?-->/g, '')
    .replace(/<[^>]*>/g, '')
    .replace(/\s+/g, ' ')
    .trim();

describe('the inbound delivery row', () => {
  it('draws a tail that adds to the title, and none that repeats it', () => {
    // A cron fire: the fold makes the title the body's first line, so the tail
    // would be the same sentence twice (Ved, 2026-10-03).
    const doubled = draw();
    expect(summaryWords(doubled), 'the fire leads with its prompt').toContain('the morning sweep');
    expect(doubled, 'and the same line is not drawn twice').not.toContain('class="ev"');
    // Once is once: the title alone, not the title plus itself.
    expect(summaryWords(doubled).match(/the morning sweep/g), 'the words once').toHaveLength(1);

    // A Slack message: a header for the title, the message's own first line as
    // the tail - the shape the tail exists for.
    const slack = draw({
      title: 'Busytools \u{b7} general \u{b7} steward',
      body: 'the gate is green\nand the queue is empty',
    });
    expect(summaryWords(slack), 'the header the title carries').toContain('Busytools');
    expect(summaryWords(slack), "and the message's own first line").toContain('the gate is green');
    expect(summaryWords(slack), 'with only the first line of it').not.toContain(
      'and the queue is empty',
    );

    // And a delivery that carried nothing draws no tail either.
    expect(draw({ body: '' }), 'a delivery with no body carries no tail').not.toContain(
      'class="ev"',
    );
  });

  it('holds the tail to one line that takes the width the title leaves', () => {
    for (const [what, sheet] of sheets()) {
      expect(sheet, `${what} clamps the tail to its one line`).toMatch(
        /details\.inboundrow > summary \.ev \{[^}]*overflow: hidden[^}]*text-overflow: ellipsis[^}]*white-space: nowrap/,
      );
      expect(sheet, `${what} gives the tail the width the title leaves`).toMatch(
        /details\.inboundrow > summary \.ev \{[^}]*flex: 1 1 0/,
      );
      // The elevated tone is the row's own arm, and it is the tone rule that
      // has to reach the same element the clamp does.
      expect(sheet, `${what} keeps the elevated tone`).toMatch(
        /details\.inboundrow > summary \.ev\.warn \{[^}]*color: var\(--warn\)/,
      );
    }
  });
});

/** The two sheets a page reads: the app's, and the book's own drawing of it. */
function sheets(): Array<[string, string]> {
  return [
    ['web.css', SHEET],
    ['the book drawing', BOOK],
  ];
}
