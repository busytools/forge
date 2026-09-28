import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { ICONS, SPRITE } from './sprite';

/** The ids a mockup asks for, as `<use href="#i-...">`. */
function idsUsedBy(mock: string): Set<string> {
  const html = readFileSync(new URL(mock, import.meta.url), 'utf8');
  return new Set([...html.matchAll(/href="#i-([a-z0-9-]+)"/g)].map((match) => match[1] as string));
}

describe('the icon sprite', () => {
  it('is the set the server ships, and every id is unique', () => {
    const ids = [...SPRITE.matchAll(/<symbol id="i-([a-z0-9-]+)"/g)].map((match) => match[1]);
    expect(ids.length).toBeGreaterThan(0);
    expect(new Set(ids).size, 'two symbols share an id').toBe(ids.length);
    expect([...ICONS].sort()).toEqual([...ids].sort());
  });

  /**
   * An icon a page draws and the sprite lacks renders as nothing, with no
   * error and no gap - the row is simply missing its glyph. The session and
   * composer mockups are what the pages that draw icons are measured
   * against, so the sprite has to cover what they ask for.
   */
  it('carries every icon the mockups draw', () => {
    for (const mock of [
      '../../../docs/mockups/web-session.html',
      '../../../docs/mockups/web-composer.html',
    ]) {
      const used = idsUsedBy(mock);
      expect(used.size, `${mock} draws no icons`).toBeGreaterThan(0);
      for (const id of used) {
        expect(ICONS, `${mock} draws #i-${id}, which the sprite does not carry`).toContain(id);
      }
    }
  });

  it('carries what the disclosure arrow asks for', () => {
    expect(ICONS).toContain('chev');
  });
});
