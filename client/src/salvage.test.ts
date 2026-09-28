import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

/** What git would name this content, which is how a copy is pinned. */
function gitBlob(content: string): string {
  return createHash('sha1')
    .update(`blob ${Buffer.byteLength(content)}\0${content}`)
    .digest('hex');
}

/**
 * The two copies the client takes from the server, and why they are pinned.
 *
 * **Prettier reformats both by default.** The sheet is byte-identical to the
 * one `forge-web` serves, and the fixtures are byte-identical to the ones the
 * server's tests pin, so a formatter run over the tree silently falsifies
 * the salvage claim. `.prettierignore` is the first defence; this is the one
 * that says so out loud, and it is what caught the move to `src/dev/fixtures`
 * leaving the ignore naming the path the files had left.
 */
describe('the salvage copies', () => {
  it("ships the server's own stylesheet, byte for byte", () => {
    expect(read('./assets/web.css')).toBe(read('../../crates/forge-web/src/web.css'));
  });

  /**
   * The hashes live in `wire/README.md` rather than here, so a re-sync has
   * one home. The count guard is deliberate: a refactor of that sentence
   * would leave this test asserting nothing at all.
   */
  it("ships the server's own fixtures, byte for byte", () => {
    const pinned = Object.fromEntries(
      [...read('./wire/README.md').matchAll(/`(\w+\.json)` is blob `([0-9a-f]{40})`/g)].map(
        (match) => [match[1] as string, match[2] as string],
      ),
    );
    expect(Object.keys(pinned).sort(), 'wire/README.md records no pinned hashes').toEqual([
      'home.json',
      'session.json',
    ]);

    for (const [name, hash] of Object.entries(pinned)) {
      expect(gitBlob(read(`./dev/fixtures/${name}`)), `${name} is not the copy it claims`).toBe(
        hash,
      );
    }
  });

  /**
   * The palette has the same two-copies problem as the sheet and, until
   * this, nothing pinned it. `theme.rs` names its values as constants and
   * lists them by key; `theme.ts` holds them inline, so the two shapes are
   * parsed apart and compared as maps.
   *
   * A value that drifts here is a token the server and the client disagree
   * about, which shows up as one surface drawing a different colour rather
   * than as a failure - the reason the sheet's copy is pinned too.
   */
  it('resolves the same palette the server does', () => {
    // The key carries a digit (`--s1`), so the character class does too.
    const KEY = /'?(--[a-z0-9-]+)'?/;

    const rust = read('../../crates/forge-web/src/theme.rs');
    const named = new Map(
      [...rust.matchAll(/const ([A-Z0-9_]+): &str = "(#[0-9a-f]{3,8})";/g)].map((match) => [
        match[1] as string,
        match[2] as string,
      ]),
    );
    const server = new Map(
      [...rust.matchAll(/\("(--[a-z0-9-]+)", ([A-Z0-9_]+)\)/g)].map((match) => [
        match[1] as string,
        named.get(match[2] as string),
      ]),
    );
    const client = new Map(
      [...read('./theme.ts').matchAll(/'(--[a-z0-9-]+)': '(#[0-9a-f]{3,8})'/g)].map((match) => [
        match[1] as string,
        match[2] as string,
      ]),
    );

    expect(server.size, 'theme.rs lists no tokens').toBeGreaterThan(15);

    /**
     * The client drops exactly the highlighter's five, because the server
     * owns no code colouring: naming them here is what stops a SIXTH one
     * disappearing from the client unnoticed, which is the drift this test
     * exists for.
     */
    const dropped = new Set(['--syn-key', '--syn-str', '--syn-fn', '--add-bg', '--del-bg']);
    expect(
      [...server.keys()].filter((token) => !client.has(token)).sort(),
      'the client carries a different set from the server',
    ).toEqual([...dropped].sort());

    for (const [token, value] of client) {
      expect(value, `${token} differs between the two copies`).toBe(server.get(token));
      expect(KEY.test(token), `${token} is not a token name`).toBe(true);
    }
  });
});
