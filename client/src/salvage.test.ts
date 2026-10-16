import { readdirSync, readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { repoPath } from './repo';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

/** Where the server writes the fixtures these are copies of. */
const SERVER_FIXTURES = repoPath('crates/forge-server/tests/wire_fixtures');

/** The book's client drawings, each with a `:root` of its own. */
const DRAWINGS = repoPath('docs/book/src/ui/client');

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
 * need. `forge-web` has gone, so the copy's provenance is this comment
 * rather than a test.
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
    const names = readdirSync(SERVER_FIXTURES)
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
   * **The drawings are copies too, and seven of them were behind.** Each page
   * under `docs/book/src/ui/client/` carries its own palette, so there is one
   * copy of it for every drawing; the pages a reader is sent to as the visual
   * truth were naming two colours no surface draws.
   *
   * The cause is per page rather than one event: the token moved and the
   * pages drawn before it kept the old pair, while the two home variants were
   * drawn after the move and were behind from their first commit.
   *
   * The comparison is against the app's own `theme.ts`, so a drawing cannot
   * agree with a stale palette. A
   * colour is compared by value rather than by spelling: `#ffb058` and
   * `rgb(255,176,88)` are one colour, and a test that read them apart would
   * fail on notation while the drawing drew the right thing.
   */
  it('draws the palette the app ships, on every drawing', () => {
    const shipped = appPalette();

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
    // 157 before the board page landed, and 171 while the board drew its rows
    // as a list. The lanes board waits its strip on `--sel`, which the page
    // now declares, so the page is one token richer: the count moving is the
    // sweep's reach moving, which is exactly what this assertion is for. 172
    // before the extensions page's drawing landed, which declares the whole
    // palette in its own `:root` like every other page, and the review
    // drawing's thirteen add to that: the two sums add, so this is the
    // merged tree's count rather than either branch's alone. 203 after the
    // home drawing declared the `--ctl` and `--blue` its rules already used;
    // its other addition, `--fs-label`, is not a palette token, so it does
    // not move the count. The document viewer's drawing declares the
    // palette's sixteen and adds the same way. 236 while the source was
    // `forge-web`'s `theme.rs`, and 231 once the app's own `theme.ts` became
    // it: the five the drawings carry and the client's map does not are the
    // highlighter's (`--syn-*`, `--add-bg`, `--del-bg`), skipped here
    // exactly as they were skipped from the other side before. 214 now that
    // the usage page is gone: its drawing declared the palette's seventeen.
    // 191 after the residue cut's two unreferenced home drawings left this
    // sweep, the queue drawing's own `--violet` adding one back. 192 after the
    // connect drawing declared the `--ctl` its button's rule draws, the
    // address field's rule staying borderless in that drawing.
    expect(compared, 'the sweep no longer reaches the same tokens').toBe(192);
  });

  /**
   * A page states the palette TWICE: as the tokens it draws with, and as the
   * legend a reader compares by eye - a swatch beside the hex. The sweep above
   * reads `:root` alone, so the legend is where a value goes stale unseen: the
   * spine page printed the old `--dim` and `--text` in its rows while its own
   * tokens were right by then.
   */
  it('prints the palette it states, on every legend row', () => {
    const shipped = appPalette();
    // A palette that failed to parse would make every comparison below skip,
    // which is the green-means-nothing shape this control exists for.
    expect(shipped.size, 'theme.ts lists no tokens').toBeGreaterThan(15);

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
    // 18 after the residue cut's two unreferenced home drawings left, six
    // rows each.
    expect(rows, 'the sweep no longer reaches every legend row').toBe(18);
  });

  /**
   * The font stacks are copied the same way. Two drawings had dropped the
   * `"Segoe UI"` step, which is the fallback a Windows reader gets, so the
   * drawing named a stack the client does not ship. Whitespace is not part of
   * a stack, so the comparison drops it.
   */
  it('draws the font stacks the app ships, on every drawing', () => {
    // The BUILT-IN face, not the `system` one: a drawing draws what a client
    // with no `[client] font` set gets, and theme.ts carries both.
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
    // 22 before the board page landed; it states the shipped stacks in its
    // `:root` like every drawing does. 24 before the extensions page's
    // drawing, which declares both stacks the same way, and the review
    // drawing's two add to both: this is the merged tree's count. The
    // document viewer's drawing declares both too; 30, the usage page's
    // drawing having gone with its page. 26, the residue cut's two
    // unreferenced home drawings having gone with theirs.
    expect(compared, 'the sweep no longer reaches every stack').toBe(26);
  });
});

/** The book's drawings, read in a fixed order so a failure names the same page twice. */
function drawings(): string[] {
  const pages = readdirSync(DRAWINGS)
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
 * The palette the app draws with, which is what a drawing has to agree with.
 *
 * **The client's own `theme.ts` is the source now.** The values used to be
 * pinned against `forge-web`'s `theme.rs`, a second place the same palette
 * lived; that crate is deleted, and the client's map is the only statement
 * of the palette left, so the drawings are held to it directly.
 */
function appPalette(): Map<string, string> {
  return new Map(
    [...read('./theme.ts').matchAll(/'(--[a-z0-9-]+)': '([^']+)'/g)].map((match) => [
      match[1] as string,
      match[2] as string,
    ]),
  );
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
