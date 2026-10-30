import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

/**
 * The page's rules that no rendered test can see: jsdom performs no layout,
 * so a finger-sized target is a fact about the sheet rather than the drawn
 * page (`Composer.test.ts` pins its own the same way).
 */
const sheet = readFileSync('src/assets/web.css', 'utf8');

describe("the settings page's rules", () => {
  it("keeps the sub-nav at a finger's size under a coarse pointer", () => {
    // The page's own section, not the sheet's first coarse block: several
    // surfaces carry one, and a pin that read the wrong block would pass on
    // another page's rules.
    const section = sheet.slice(sheet.indexOf('/* ---------- the settings page ---------- */'));
    expect(section, 'the section was found at all').not.toBe('');
    const start = section.indexOf('@media (pointer: coarse) {');
    expect(start, 'the section carries a coarse block').toBeGreaterThan(-1);
    const next = section.indexOf('@media', start + 1);
    const coarse = section.slice(start, next === -1 ? section.length : next);

    const rail = /\.settings-page \.settings-rail-item[^{}]*\{[^{}]*min-height:\s*44px/.exec(
      coarse,
    )?.[0];
    expect(rail ?? '', 'the sub-nav lost its height floor on a finger').not.toBe('');

    const buttons = /\.settings-page \.settings-btn[^{}]*\{[^{}]*min-height:\s*44px/.exec(
      coarse,
    )?.[0];
    expect(buttons ?? '', 'the save bar lost its height floor on a finger').not.toBe('');
    const boxes = /\.settings-page \.settings-rows input[^{}]*\{[^{}]*min-height:\s*44px/.exec(
      coarse,
    )?.[0];
    expect(boxes ?? '', 'the editor boxes lost their height floor on a finger').not.toBe('');
  });
});
