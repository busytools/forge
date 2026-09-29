import { readdirSync, readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

/** Where the server writes the fixtures these are copies of. */
const SERVER_FIXTURES = '../../crates/forge-server/tests/wire_fixtures';

/**
 * The copy the client takes from the server, and why it is pinned.
 *
 * **Prettier reformats it by default.** The fixtures are byte-identical to the
 * ones the server's tests pin, so a formatter run over the tree silently
 * falsifies the claim. `.prettierignore` is the first defence; this is the one
 * that says so out loud, and it is what caught the move to `src/dev/fixtures`
 * leaving the ignore naming the path the files had left.
 *
 * **The stylesheet is not pinned, because it is not the server's any more.**
 * It began as a byte-identical copy of `crates/forge-web/src/web.css`, and
 * that assertion also forbade correcting it - which is what a page's rules
 * need. `forge-web` goes when the client plan reaches its delete step, so the
 * copy's provenance is this comment rather than a test.
 */
describe('the salvage copies', () => {
  /**
   * **The comparison is against the file the SERVER writes, and that is the
   * whole point of it.** It used to compare each copy against a hash written
   * in `wire/README.md`, so a wire reshaped on the server and not re-synced
   * here left the suite green: the copy and the hash it was checked against
   * both still said the old shape, and nothing in the client ever read
   * `crates/forge-server/tests/wire_fixtures/` at all. A pin that has to be
   * updated by hand cannot catch a copy that was not.
   *
   * The names are read from the server's directory rather than listed here, so
   * a fixture the server starts writing is covered the day it lands.
   */
  it("ships the server's own fixtures, byte for byte", () => {
    const names = readdirSync(new URL(SERVER_FIXTURES, import.meta.url))
      .filter((name) => name.endsWith('.json'))
      .sort();

    // A control: a directory that could not be read would make the loop below
    // pass for ever, which is a green that means the test is broken.
    expect(names, 'the server writes no fixture to compare against').not.toHaveLength(0);

    for (const name of names) {
      expect(read(`./dev/fixtures/${name}`), `${name} is not the copy the server writes`).toBe(
        read(`${SERVER_FIXTURES}/${name}`),
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
   * than as a failure - the same two-copies problem the fixtures have, and
   * the same reason they are pinned.
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
