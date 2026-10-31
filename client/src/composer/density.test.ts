import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The band's height, kept from growing by a rule.
 *
 * **Read off the sheet, not the DOM.** jsdom performs no layout, so no
 * rendered assertion can see a height; what is pinned here is the declared
 * arithmetic the measured heights rest on - the live row's padding and the
 * body row's, the coarse-pointer floor on every control, and the row's own
 * clip rather than a wrap. The measured heights themselves (82px idle on a
 * live seat, 46px on a quiet one, 100px at a phone) are in the book's drawing
 * and were taken in a browser.
 */
const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = SHEET.indexOf(`${selector} {`);
  expect(at, `the sheet has no rule for ${selector}`).toBeGreaterThan(-1);
  return SHEET.slice(at, SHEET.indexOf('}', at));
}

/** A declaration's value, from the rule's own text. */
function value(declared: string, property: string): string {
  return new RegExp(`(?:^|[;{])\\s*${property}:\\s*([^;]+)`).exec(declared)?.[1]?.trim() ?? '';
}

describe("the band's height", () => {
  it('keeps the live row one line of the measuring padding', () => {
    // A taller row is the band growing for a chip: the figures and the chips
    // are one line, and the padding is the mockup's own measure.
    const row = rule('.live-row');
    expect(value(row, 'padding'), 'the live row grew a padding nothing asked for').toBe(
      '5px 10px 5px 8px',
    );
    expect(value(row, 'flex-wrap'), 'the row wraps again').toBe('nowrap');
    expect(value(row, 'overflow'), 'the row scrolls sideways again').toBe('hidden');
  });

  it('draws the body row at the sheet field padding, not a second lane', () => {
    const line = rule('.line');
    expect(value(line, 'padding'), 'the body row grew a padding nothing asked for').toBe(
      '10px 12px 10px 14px',
    );
    // The controls ride this row; a column of their own was the second row.
    expect(value(rule('.ctls'), 'margin-left'), 'the controls left the row').toBe('auto');
  });

  it('takes every finger target on the row to the 44px floor', () => {
    // The phone's band is 100px because the row's controls are 44px: a floor
    // dropped here is a target a finger misses, not a smaller band.
    const coarse = [...SHEET.matchAll(/@media \(pointer: coarse\) \{\n([\s\S]*?)\n\}/g)]
      .map((match) => match[1])
      .join('\n');
    expect(coarse, 'the live row lost its finger-sized chips').toContain(
      '.live-row .live-more, .live-row .live-door,',
    );
    expect(coarse, 'the live row lost the floor its chips and doors take').toMatch(
      /\.live-row \.sg-tog, \.live-row \.bz-tog \{ min-height: 44px; \}/,
    );
  });
});
