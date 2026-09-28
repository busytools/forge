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
});
