import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

/**
 * The page's rules that no rendered test can see: jsdom performs no layout, so
 * a finger-sized target, the row heights and the edit door's reachability are
 * facts about the sheet rather than the drawn page (`Composer.test.ts` pins
 * its own the same way).
 */
const sheet = readFileSync('src/assets/web.css', 'utf8');

/** The page's own section, not the sheet's first block: several surfaces
 *  carry one, and a pin that read the wrong block would pass on another
 *  page's rules. */
const section = sheet.slice(sheet.indexOf('/* ---------- the settings page ---------- */'));

/** One media block from the page's section, up to the next at-rule. */
function media(rule: string): string {
  const start = section.indexOf(rule);
  expect(start, `the section carries ${rule}`).toBeGreaterThan(-1);
  const next = section.indexOf('@media', start + 1);
  return section.slice(start, next === -1 ? section.length : next);
}

describe("the settings page's rules", () => {
  it("keeps the page's controls at a finger's size under a coarse pointer", () => {
    const coarse = media('@media (pointer: coarse) {');
    expect(
      /\.settings-page \.settings-rail-item[^{}]*\{[^{}]*min-height:\s*44px/.test(coarse),
      'the sub-nav lost its height floor on a finger',
    ).toBe(true);
    expect(
      /\.settings-btn[^{}]*\{[^{}]*min-height:\s*44px/.test(coarse),
      'the save bar lost its height floor on a finger',
    ).toBe(true);
    expect(
      /\.settings-page \.settings-setting input[^{}]*\{[^{}]*min-height:\s*44px/.test(coarse),
      'the editor boxes lost their height floor on a finger',
    ).toBe(true);
    expect(
      /\.settings-page \.settings-mode[^{}]*\{[^{}]*min-height:\s*44px/.test(coarse),
      'the edit door lost its height floor on a finger',
    ).toBe(true);
  });

  it('draws the setting line at 40px and its editing row at 48px', () => {
    expect(
      /\.settings-page \.settings-setting[^{}]*\{[^{}]*min-height:\s*40px/.test(section),
      'the setting line is not 40px',
    ).toBe(true);
    expect(
      /\.settings-page \.settings-setting\.edit[^{}]*\{[^{}]*min-height:\s*48px/.test(section),
      'the editing line is not 48px',
    ).toBe(true);
  });

  /**
   * **The door is the only way into edit mode.** It was `display: none` below
   * 760px, so the editor could not be reached on a phone at all.
   */
  it('keeps the edit door at 44px and never hides it', () => {
    expect(
      /\.settings-page \.settings-mode[^{}]*\{[^{}]*min-height:\s*44px/.test(section),
      'the edit door has no 44px floor at every width',
    ).toBe(true);
    expect(
      /\.settings-mode[^{}]*\{[^{}]*display:\s*none/.test(section),
      'a rule hides the only way into edit mode',
    ).toBe(false);
  });
});
