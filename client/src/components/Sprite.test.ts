import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { ICONS, SPRITE } from './sprite';

/** The ids a mockup asks for, as `<use href="#i-...">`. */
function idsUsedBy(mock: string): Set<string> {
  const html = readFileSync(new URL(mock, import.meta.url), 'utf8');
  return new Set([...html.matchAll(/href="#i-([a-z0-9-]+)"/g)].map((match) => match[1] as string));
}

describe('the icon sprite', () => {
  /**
   * Against `icons.rs`, not against the sprite. Deriving both sides from the
   * same string with the same pattern is an assertion that holds for any
   * sprite at all, including one missing a symbol the pages draw.
   *
   * **`icons.rs` is the source of record while that crate lives.** It goes
   * with `forge-web`, and this comparison goes with it.
   */
  it('carries the ids the Rust sprite defines, and every one is unique', () => {
    const rust = readFileSync(
      new URL('../../../crates/forge-web/src/icons.rs', import.meta.url),
      'utf8',
    );
    const shipped = [...rust.matchAll(/<symbol id="i-([a-z0-9-]+)"/g)].map((match) => match[1]);
    expect(shipped.length, 'icons.rs defines no symbols').toBeGreaterThan(0);
    expect(new Set(ICONS).size, 'two symbols share an id').toBe(ICONS.length);
    expect([...ICONS].sort(), 'the sprite and icons.rs disagree').toEqual([...shipped].sort());
  });

  it('matches the Rust payload byte for byte', () => {
    const rust = readFileSync(
      new URL('../../../crates/forge-web/src/icons.rs', import.meta.url),
      'utf8',
    );
    const payload = /const SPRITE: &str = r#"(.*?)"#;/s.exec(rust)?.[1];
    expect(payload, 'icons.rs has no SPRITE const to compare against').toBeDefined();
    expect(SPRITE).toBe(payload);
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
