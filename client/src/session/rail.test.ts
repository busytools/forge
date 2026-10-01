import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The rail's own rules, read off the sheet by name.
 *
 * **Why this level.** The defect is a DECLARATION - which way a fold's summary
 * aligns its items - and jsdom performs no layout, so nothing rendered can see
 * it. The by-width measurement in the pull request is what says the chevron
 * lands where the rule asks; this says the rule is the one it was fixed to.
 */

const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = sheet.indexOf(`${selector} {`);
  expect(at, `${selector} is in the sheet`).toBeGreaterThan(-1);
  const end = sheet.indexOf('}', at);
  return sheet.slice(at, end);
}

describe('the asleep heading', () => {
  it('centres its chevron on the row rather than on the text baseline', () => {
    // `baseline` rides the icon on the label's baseline, which drew it about
    // 4px above the row's centre - the one fold on the page that did.
    const gfold = rule('details.gfold > summary');
    expect(gfold).toContain('align-items: center');
    expect(gfold, 'and not the baseline it was drawn on').not.toContain('align-items: baseline');
  });
});
