import { readdirSync, readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

/** Where the server writes the fixtures these are copies of. */
const SERVER_FIXTURES = '../../crates/forge-server/tests/wire_fixtures';

/** The book's client drawings, each with a `:root` of its own. */
const DRAWINGS = '../../docs/book/src/ui/client';

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

  /**
   * **The drawings are copies too, and seven of them were behind.** Each page
   * under `docs/book/src/ui/client/` carries its own palette, so there is one
   * copy of it for every drawing; the pages a reader is sent to as the visual
   * truth were naming two colours no surface draws.
   *
   * The cause is per page rather than one event: the token moved and the
   * pages drawn before it kept the old pair, while the two home variants were
   * drawn after the move and were behind from their first commit.
   *
   * The comparison is against the SERVER's palette, the same source the copy
   * above is pinned to, so a drawing cannot agree with a stale client. A
   * colour is compared by value rather than by spelling: `#ffb058` and
   * `rgb(255,176,88)` are one colour, and a test that read them apart would
   * fail on notation while the drawing drew the right thing.
   */
  it('draws the palette the app ships, on every drawing', () => {
    const shipped = serverPalette();

    let compared = 0;
    const drifts: string[] = [];
    for (const page of drawings()) {
      for (const [token, value] of declaredIn(page)) {
        const source = shipped.get(token);
        if (source === undefined) continue;
        compared += 1;
        if (rgb(value) !== rgb(source)) {
          drifts.push(`${page}: ${token} is ${value}, the app ships ${source}`);
        }
      }
    }

    // Every drift at once rather than the first: a page behind on two tokens
    // is one edit, and a run that stopped at the first would take as many
    // rounds as there are tokens.
    expect(drifts, 'drawings name colours the app does not ship').toEqual([]);

    // The denominator, EXACT: `>100` passes with a page missing, because the
    // pages carry different numbers of tokens. A count that moves means the
    // sweep's reach moved - a page gone, or a page that stopped declaring a
    // palette - and both are the drift this test exists for.
    expect(compared, 'the sweep no longer reaches the same tokens').toBe(125);
  });

  /**
   * A page states the palette TWICE: as the tokens it draws with, and as the
   * legend a reader compares by eye - a swatch beside the hex. The sweep above
   * reads `:root` alone, so the legend is where a value goes stale unseen: the
   * spine page printed the old `--dim` and `--text` in its rows while its own
   * tokens were right by then.
   */
  it('prints the palette it states, on every legend row', () => {
    const shipped = serverPalette();

    let rows = 0;
    const drifts: string[] = [];
    for (const page of drawings()) {
      for (const row of legendRows(page)) {
        const source = shipped.get(row.token);
        if (source === undefined) continue;
        rows += 1;
        for (const [what, value] of [
          ['swatch', row.swatch],
          ['hex', row.hex],
        ] as const) {
          if (rgb(value) !== rgb(source)) {
            drifts.push(`${page}: ${row.token}'s ${what} is ${value}, the app ships ${source}`);
          }
        }
      }
    }

    expect(drifts, 'a legend prints a colour the app does not ship').toEqual([]);
    expect(rows, 'the sweep no longer reaches every legend row').toBe(30);
  });

  /**
   * The font stacks are copied the same way. Two drawings had dropped the
   * `"Segoe UI"` step, which is the fallback a Windows reader gets, so the
   * drawing named a stack the client does not ship. Whitespace is not part of
   * a stack, so the comparison drops it.
   */
  it('draws the font stacks the app ships, on every drawing', () => {
    // The BUILT-IN face, not the `system` one: a drawing draws what a client
    // with no `[web] font` set gets, and theme.ts carries both.
    const builtIn = /const BUILT_IN_FONT = \{([\s\S]*?)\};/.exec(read('./theme.ts'))?.[1] ?? '';
    const shipped = new Map(
      [...builtIn.matchAll(/(ui|mono): '([^']+)'/g)].map((match) => [
        match[1] as string,
        match[2] as string,
      ]),
    );
    expect(shipped.size, 'theme.ts carries no built-in stack to compare').toBe(2);

    let compared = 0;
    const drifts: string[] = [];
    for (const page of drawings()) {
      const declared = declaredIn(page);
      for (const [token, name] of [
        ['--ui', 'ui'],
        ['--mono', 'mono'],
      ] as const) {
        const value = declared.get(token);
        if (value === undefined) continue;
        compared += 1;
        if (stack(value) !== stack(shipped.get(name) ?? '')) {
          drifts.push(`${page}: ${token} is ${value}, the app ships ${shipped.get(name)}`);
        }
      }
    }

    expect(drifts, 'drawings name stacks the app does not ship').toEqual([]);
    expect(compared, 'the sweep no longer reaches every stack').toBe(18);
  });
});

/** The book's drawings, read in a fixed order so a failure names the same page twice. */
function drawings(): string[] {
  const pages = readdirSync(new URL(DRAWINGS, import.meta.url))
    .filter((name) => /^web-.*\.html$/.test(name))
    .sort();
  // A directory that could not be read would make every sweep below pass for
  // ever, which is a green that means the check is broken.
  expect(pages, 'no drawing declares anything to compare').not.toHaveLength(0);
  return pages;
}

/**
 * The tokens a drawing declares, in the drawing's own spelling.
 *
 * **Every `:root` block, not the first.** Three pages carry a second one - it
 * holds `--pad` today - and a sweep that read only the first would be blind to
 * a palette token moved into it, which is the same shape as the legend rows
 * the first version of this sweep could not see.
 */
function declaredIn(page: string): Map<string, string> {
  const declared = new Map<string, string>();
  for (const block of read(`${DRAWINGS}/${page}`).matchAll(/:root\s*\{([\s\S]*?)\n\s*\}/g)) {
    for (const token of (block[1] as string).matchAll(/(--[a-z0-9-]+)\s*:\s*([^;]+);/g)) {
      declared.set(token[1] as string, token[2] as string);
    }
  }
  return declared;
}

/**
 * The palette the server owns, which is what a drawing has to agree with.
 *
 * **It reads the PARKED crate, and that is safe only transitively**: the test
 * above pins `theme.ts` to this file, so the two agree today. `forge-web` is
 * deleted when the client plan reaches its delete step, and that deletion has
 * to repoint this at `client/src/theme.ts` in the same change. It fails loudly
 * rather than silently if it does not - the file would not be there to read -
 * but the drawings would be unguarded until someone repoints it.
 */
function serverPalette(): Map<string, string> {
  const rust = read('../../crates/forge-web/src/theme.rs');
  const named = new Map(
    // Any literal, not only a hex one: two of the tokens are `rgba(...)`, and a
    // pattern that read only hex would skip them instead of comparing them.
    [...rust.matchAll(/const ([A-Z0-9_]+): &str = "([^"]+)";/g)].map((match) => [
      match[1] as string,
      match[2] as string,
    ]),
  );
  const pairs: [string, string][] = [];
  for (const match of rust.matchAll(/\("(--[a-z0-9-]+)", ([A-Z0-9_]+)\)/g)) {
    const value = named.get(match[2] as string);
    // A token whose constant the file does not declare is dropped rather than
    // carried as an empty string, which would read as a drift in every page.
    if (value !== undefined) pairs.push([match[1] as string, value]);
  }
  return new Map(pairs);
}

/** One row of a page's palette legend: the token, its swatch and its hex. */
function legendRows(page: string): { token: string; swatch: string; hex: string }[] {
  const rows: { token: string; swatch: string; hex: string }[] = [];
  const pattern =
    /class="tok"><span class="sw" style="background:([^"]+)"><\/span><code>(--[a-z0-9-]+)<\/code><span class="hx">([^<]+)</g;
  for (const match of read(`${DRAWINGS}/${page}`).matchAll(pattern)) {
    rows.push({
      swatch: match[1] as string,
      token: match[2] as string,
      hex: (match[3] as string).trim(),
    });
  }
  return rows;
}

/** A colour as one spelling, so `#ffb058` and `rgb(255,176,88)` compare equal. */
function rgb(value: string): string {
  const hex = /^#([0-9a-f]{6})$/i.exec(value.trim());
  if (hex === null) return value.replace(/\s+/g, '').toLowerCase();
  const [r, g, b] = [0, 2, 4].map((at) =>
    Number.parseInt((hex[1] as string).slice(at, at + 2), 16),
  );
  return `rgb(${r},${g},${b})`;
}

/** A font stack with its quoting and spacing dropped, which do not change it. */
function stack(value: string): string {
  return value.replace(/["'\s]/g, '').toLowerCase();
}
