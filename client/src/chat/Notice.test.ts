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

/**
 * Every selector member the sheet writes whose FIRST compound names `.notice`
 * - the unscoped form, however it is dressed: the bare rule, a variant, a
 * type-dressed one like `div.notice`, a sibling. A member whose `.notice` sits
 * behind a scope (`.conv .notice`) carries it in a later compound and is fine.
 */
function unscopedNotice(): string[] {
  const found: string[] = [];
  const code = SHEET.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const rule of code.matchAll(/([^{}]+)\{/g)) {
    for (const member of (rule[1] ?? '').split(',')) {
      const one = member.trim();
      const first = one.split(/\s+|[>+~]/)[0] ?? '';
      if (first.includes('.notice')) found.push(one);
    }
  }
  return found;
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
    expect(unscopedNotice(), 'and no selector names .notice without a scope').toEqual([]);
  });
});
