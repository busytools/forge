import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The width the card's graph draws its bars at.
 *
 * **Where this check can look, and why.** What the bars SPAN is a question
 * about layout, and jsdom performs none, so no rendered assertion can see it:
 * this reads the declaration off the sheet by name, the way `density.test.ts`
 * and `Session.test.ts` read the ones they pin. What a browser paints is
 * measured by width in the pull request, not here.
 *
 * The graph's track is `client/src/assets/web.css`'s `.tc .bars` and the bars
 * are `.tc .bars i`, one per reading over `CARD_CELLS` (40 of them). The track
 * is the card's own fixed width, so the bars share it rather than sizing to
 * the box: the card's ladder removes the graph at narrow widths instead of
 * squeezing it, and the readings it holds are the newest forty whatever the
 * track is doing.
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
  const declared = /(?:^|[;{])\s*flex:\s*([^;]+)/.exec(rule('.tc .bars i'))?.[1] ?? '';
  const [grow, shrink, basis] = declared.trim().split(/\s+/);
  return { grow: Number(grow), shrink: Number(shrink), basis: basis ?? '' };
}

describe("the card graph's bars", () => {
  it('share the width their track leaves over, rather than leaving it undrawn', () => {
    const { grow, basis } = flex();
    expect(grow, 'the bars do not take the width the track leaves over').toBeGreaterThan(0);
    expect(basis, 'the width the leftover is shared out from is the bar itself').toBe('1px');
  });

  it('hold their own width, so the window never squeezes into the track', () => {
    expect(flex().shrink, 'a shrink squeezes the window into a track it cannot fit').toBe(0);
  });

  it('holds the track at the card width, which the ladder removes rather than resizes', () => {
    const track = rule('.tc .bars');
    expect(track, 'the track sizes itself to its bars').toContain('width: 116px');
    expect(track, 'the track takes the box width').toContain('flex: none');
    expect(SHEET, 'the graph is squeezed instead of removed when the card narrows').toMatch(
      /@container \(max-width: \d+px\) \{ \.tc \.bars,[^{]*\{ display: none; \} \}/,
    );
  });
});
