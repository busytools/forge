import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { repoPath } from '../repo';
import { ICONS, SPRITE } from './sprite';

/**
 * Every first capture group. `noUncheckedIndexedAccess` types an indexed
 * match as `string | undefined` because it cannot know the pattern has a
 * group; the loop says both that it does and that this code does not assume
 * it.
 */
function captures(text: string, pattern: RegExp): string[] {
  const found: string[] = [];
  for (const match of text.matchAll(pattern)) {
    const group = match[1];
    if (group !== undefined) found.push(group);
  }
  return found;
}

/** The ids a mockup asks for, as `<use href="#i-...">`. */
function idsUsedBy(mock: string): Set<string> {
  const html = readFileSync(repoPath(mock), 'utf8');
  return new Set(captures(html, /href="#i-([a-z0-9-]+)"/g));
}

describe('the icon sprite', () => {
  /**
   * The sprite is the client's own asset now: its values were salvaged from
   * `forge-web`'s `icons.rs` before that crate was deleted, and the two pins
   * that compared against the Rust source went with it. What remains is what
   * still has a subject - the ids are unique, and the drawing below proves
   * the set is not empty by requiring every icon a page asks for.
   */
  it('gives every symbol an id of its own', () => {
    expect(new Set(ICONS).size, 'two symbols share an id').toBe(ICONS.length);
    expect(SPRITE.length, 'the sprite is empty').toBeGreaterThan(0);
  });

  /**
   * An icon a page draws and the sprite lacks renders as nothing, with no
   * error and no gap - the row is simply missing its glyph. The session and
   * composer mockups are what the pages that draw icons are measured
   * against, so the sprite has to cover what they ask for.
   */
  it('carries every icon the mockups draw', () => {
    for (const mock of [
      'docs/book/src/ui/client/web-session.html',
      'docs/book/src/ui/client/web-composer.html',
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

  /**
   * **The three notice levels are three shapes.** A notice carries its level
   * on the glyph alone, so a sprite that copied info under `i-warning` would
   * paint every level as the same circle - the id would exist and the row
   * would read wrong.
   */
  it('draws a different shape for each notice level', () => {
    const body = (id: string): string => {
      const symbol = new RegExp(`<symbol id="i-${id}"[^>]*>([\\s\\S]*?)</symbol>`).exec(SPRITE);
      return symbol?.[1] ?? '';
    };
    const shapes = ['info', 'warning', 'error'].map((id) => {
      const shape = body(id);
      expect(shape.length, `the sprite carries no i-${id}`).toBeGreaterThan(0);
      return shape;
    });
    expect(new Set(shapes).size, 'two levels draw the same shape').toBe(3);
  });
});
