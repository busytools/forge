import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The width the meter's bars are drawn at.
 *
 * **Where this check can look, and why.** What the bars SPAN is a question
 * about layout, and jsdom performs none, so no rendered assertion can see it:
 * this reads the declaration off the sheet by name, the way `density.test.ts`
 * and `Session.test.ts` read the ones they pin. What a browser paints is
 * measured by width in the pull request, not here.
 *
 * The row's own box is `client/src/assets/web.css`'s `.wave`; the cells are
 * `.wtr i`, one per reading, and the window is the server's `METER_CELLS` -
 * 120 of them, which at a cell's own 4px plus the 2px gap covers a little
 * over 700px. Past that the bars used to stop short of their own box and the
 * level graph ended mid-row; the sheet now shares the leftover across them.
 */

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = SHEET.indexOf(`${selector} {`);
  expect(at, `the sheet has no rule for ${selector}`).toBeGreaterThan(-1);
  return SHEET.slice(at, SHEET.indexOf('}', at));
}

/** `flex`, as the three things it sets. */
function flex(): { grow: number; shrink: number; basis: string } {
  const declared = /(?:^|[;{])\s*flex:\s*([^;]+)/.exec(rule('.wtr i'))?.[1] ?? '';
  const [grow, shrink, basis] = declared.trim().split(/\s+/);
  return { grow: Number(grow), shrink: Number(shrink), basis: basis ?? '' };
}

describe("the meter's bars", () => {
  it('share the width their box leaves over, rather than leaving it undrawn', () => {
    const { grow, basis } = flex();
    // Measured before this: a 976.11px box at the live window width drew 718px
    // of bars and left 258px of itself empty, because each cell held its own
    // 4px whatever the box had room for.
    expect(grow, 'the bars do not take the width the box leaves over').toBeGreaterThan(0);
    expect(basis, 'the width the leftover is shared out from is the bar itself').toBe('4px');
  });

  it('hold their own width in a box narrower than the window, which clips instead', () => {
    // The window is 120 cells, and at 430 the box is 382px: they cannot fit.
    // Shrinking them would draw 120 hairlines and hide that the history is
    // clipped at the left, which is what the row does at that width.
    expect(flex().shrink, 'a shrink squeezes the window into a box it cannot fit').toBe(0);
  });
});
