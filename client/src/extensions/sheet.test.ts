import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

/**
 * The page's rules that no rendered test can see: jsdom performs no layout,
 * so a finger-sized target is a fact about the sheet rather than about the
 * drawn page (`Composer.test.ts` pins its own the same way).
 */
const sheet = readFileSync('src/assets/web.css', 'utf8');

describe("the extensions page's rules", () => {
  it("keeps the way back at a finger's size under a coarse pointer", () => {
    // The page's own section, not the sheet's first coarse block: several
    // surfaces carry one, and a pin that read the wrong block would pass on
    // another page's rules.
    const section = sheet.slice(sheet.indexOf('the extensions page'));
    expect(section, 'the section was found at all').not.toBe('');
    const start = section.indexOf('@media (pointer: coarse) {');
    expect(start, 'the section carries a coarse block').toBeGreaterThan(-1);
    const next = section.indexOf('@media', start + 1);
    const coarse = section.slice(start, next === -1 ? section.length : next);

    // The grouped selector list carries the floor, so the pattern reads the
    // selector and the body together: a `.back` that survived the list
    // without its own rule would still be a smaller target than the rest.
    const back = /\.extensions-page \.back[^{}]*\{[^{}]*min-height:\s*44px/.exec(coarse)?.[0] ?? '';
    expect(back, 'the way back lost its height floor on a finger').not.toBe('');

    // **The floor is inert without this.** `.back` is an anchor, and
    // `min-height` is ignored on non-replaced inline boxes: only a
    // flex/inline-block display makes the 44px bite, so the pin requires
    // the same rule to carry one.
    const bites =
      /\.extensions-page \.back[^{}]*\{[^{}]*display:\s*(inline-flex|inline-block)[^{}]*\}/.exec(
        coarse,
      )?.[0] ?? '';
    expect(
      bites,
      'the floor on the way back no longer bites (no flex/inline-block display)',
    ).not.toBe('');
  });
});
