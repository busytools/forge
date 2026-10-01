import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The sheet, as text: what the chat's notice chrome is scoped to is a class
 * question, and jsdom does no layout, so a rendered assertion cannot see it.
 */
const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** Every rule body the sheet writes for this exact selector. */
function rulesFor(selector: string): string[] {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return [...SHEET.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))].map(
    (match) => match[1] ?? '',
  );
}

describe('the chat notice chrome, as the sheet scopes it', () => {
  it('scopes every notice rule to the conversation, so no other surface picks the name up', () => {
    // The composer's notice row drew this chrome once, because the name was
    // bare and the chat's rule reached it - 60px tall where the specimen draws
    // 22. The composer's half was fixed by the row stating its own chrome;
    // this is the other half, so a third surface cannot pick the name up the
    // same way.
    for (const selector of [
      '.conv .notice',
      '.conv .notice .sev',
      '.conv .notice.info',
      '.conv .notice.warn',
      '.conv .notice.err',
    ]) {
      expect(rulesFor(selector).length, `${selector} is written`).toBeGreaterThan(0);
    }
    expect(SHEET, 'and no rule writes a bare .notice').not.toMatch(/^\s*\.notice\s*\{/m);
  });
});
