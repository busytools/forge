/**
 * Contrast over the palette, at the token level.
 *
 * `a11y.test.ts` runs axe over the rendered pages and says why it cannot
 * answer this: jsdom performs no layout, so axe's `color-contrast` comes
 * back INCOMPLETE, and that file's clean result "is not a claim about
 * contrast". The standard puts contrast where the token set is, and this is
 * the check that sits there.
 *
 * **What this covers, and what it does not.** It compares palette tokens to
 * each other, at the WCAG ratio for what the sheet draws them as. A colour
 * written inline in a component rather than read from the token set is
 * invisible here, and nothing here renders, so this says nothing about what
 * a browser paints once a ground arrives from a gradient or an alpha layer
 * over a token - `body` lays two radial gradients over `--bg`, and `.mine`
 * one over the page. A check read as broader than it is is worse than no
 * check.
 */

import { describe, expect, it } from 'vitest';

import { rootTokens } from './theme';

/** WCAG 2.1 AA floors: 4.5:1 for text, 3:1 for a mark that is not text. */
const TEXT = 4.5;
const MARK = 3;

/** A foreground token, the ground it is drawn on, and the floor it takes. */
type Pair = readonly [foreground: string, ground: string, floor: number];

/** WCAG 2.1 relative luminance of a `#rrggbb` value. */
function luminance(hex: string): number {
  const channel = (at: number): number => {
    const c = Number.parseInt(hex.slice(at, at + 2), 16) / 255;
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
}

/** WCAG 2.1 contrast between two `#rrggbb` values: 1 at parity, 21 at worst. */
function contrast(a: string, b: string): number {
  const x = luminance(a);
  const y = luminance(b);
  const [hi, lo] = x > y ? [x, y] : [y, x];
  return (hi + 0.05) / (lo + 0.05);
}

/** The only value shape `luminance` reads. */
const OPAQUE = /^#[0-9a-f]{6}$/i;

/**
 * What a failure should say: the two tokens and both ratios.
 *
 * A pair the arithmetic cannot be run on is reported rather than measured,
 * which is the whole of what keeps a `NaN` out: `NaN < floor` is false, so
 * anything reaching `contrast` unmeasured would read as fine. Both halves
 * are real - a token renamed in `theme.ts` leaves the table stale, and a
 * value `salvage.test.ts` accepts from `theme.rs` (`#abc`, an eight-digit
 * hex, `rgba(...)`) is not a `#rrggbb` pair.
 */
function unreadable(pairs: readonly Pair[], palette: Readonly<Record<string, string>>): string[] {
  const below: string[] = [];
  for (const [foreground, ground, floor] of pairs) {
    const fg = palette[foreground];
    const bg = palette[ground];
    if (fg === undefined || bg === undefined) {
      below.push(`${foreground} on ${ground}: the palette resolves no such token`);
      continue;
    }
    if (!OPAQUE.test(fg) || !OPAQUE.test(bg)) {
      below.push(`${foreground} on ${ground}: not a pair of #rrggbb values`);
      continue;
    }
    const measured = contrast(fg, bg);
    if (measured < floor) {
      below.push(
        `${foreground} on ${ground} measures ${measured.toFixed(2)}:1, below the ${floor}:1 floor`,
      );
    }
  }
  return below;
}

/**
 * Every pair the palette has to carry: the foreground token, the ground
 * under it, and the floor that applies.
 *
 * The grounds are the token surfaces the sheet and the components draw on -
 * the page (`--bg`), a popover, a card or the connect screen's own field
 * (`--s1`), a raised chip, an inline code ground, a diff header or a button
 * (`--s2`), and a progress track (`--s3`). Every pair but `--violet`'s comes
 * from a rule, in `assets/web.css` or in a component's own style block.
 *
 * Two rows name a ground darker than the one the page actually has, which is
 * the exception the limits above describe: `--blue` is `.opt .ic.ed` inside
 * the dock's own gradient, where it measures 6.80-7.08 rather than the 7.38
 * the page gives, and `--hot` is `.dict .db` inside the composer's, at
 * 10.21-10.64 rather than 11.25. Both clear their floor there, but the
 * table's number is not the page's. `--ctl` is a third: `.opt .box2` draws it
 * on the dock's gradient, where it measures 3.23-3.36 rather than the 3.50
 * the page gives. `--ctl` on `--sel` is the thinnest margin of the three, at
 * 3.10, which is the pair to watch if either value moves.
 *
 * `TEXT` is the floor on all but one row. Nothing here takes the 3:1
 * large-text floor: the sheet's largest is `.brand .word` at 22px and weight
 * 650, neither 24px nor bold. The `MARK` row is a bar fill inside a track,
 * which is not text at all.
 *
 * `--violet` is drawn by nothing - no rule in the sheet reads it - and is
 * pinned against the page so the token cannot sit in the palette with no
 * answer for where a surface would put it.
 */
const DRAWN: readonly Pair[] = [
  ['--text', '--bg', TEXT],
  ['--text', '--s1', TEXT],
  ['--text', '--s2', TEXT],
  ['--muted', '--bg', TEXT],
  ['--muted', '--s1', TEXT],
  ['--muted', '--s2', TEXT],
  ['--dim', '--bg', TEXT],
  ['--dim', '--s1', TEXT],
  ['--dim', '--s2', TEXT],
  ['--dim', '--sel', TEXT],
  // A control's own mark, on the page and on the selected row's ground.
  ['--ctl', '--bg', MARK],
  ['--ctl', '--sel', MARK],
  ['--accent', '--bg', TEXT],
  ['--accent', '--s1', TEXT],
  ['--accent', '--s2', TEXT],
  ['--accent', '--s3', MARK],
  ['--ok', '--bg', TEXT],
  ['--ok', '--s1', TEXT],
  ['--warn', '--bg', TEXT],
  ['--warn', '--s1', TEXT],
  ['--bad', '--bg', TEXT],
  ['--bad', '--s1', TEXT],
  ['--blue', '--bg', TEXT],
  ['--teal', '--bg', TEXT],
  ['--teal', '--s2', TEXT],
  ['--violet', '--bg', TEXT],
  ['--hot', '--bg', TEXT],
];

/**
 * The palette's one token with no pair. `--line` is the hairline: section
 * rules, code frames and popover dividers. Nothing is drawn on it, and a
 * separator is not information WCAG requires to be perceived - the single
 * rule that inks it, `.sess .facts .sep`, is a middot at 1.42:1.
 *
 * A border that identifies a control is a different job and no longer this
 * token's: it is `--ctl`, carried above at the 3:1 of 1.4.11. Named rather
 * than left out, so a token arriving without a pair still fails.
 */
const NOT_INK = ['--line'];

/**
 * Two grounds one step apart, which is the shape a wrong entry in the table
 * takes. The control below measures it and must report it.
 */
const UNREADABLE: Pair = ['--s1', '--s2', TEXT];

describe('the palette', () => {
  /**
   * **The file's control.** A check that reports nothing reads the same
   * whether the palette is clean or the ratio was never computed, so one
   * pair is measured that nothing could read. Without this, the empty list
   * below is not evidence of anything.
   */
  it('finds a pair too close to read', () => {
    expect(unreadable([UNREADABLE], rootTokens(null)), 'a pair one ground apart').toHaveLength(1);
  });

  /** A pair naming a token the palette does not resolve is a stale pair. */
  it('finds a pair naming a token the palette has lost', () => {
    expect(unreadable([['--gone', '--bg', TEXT]], rootTokens(null)), 'a renamed token').toEqual([
      '--gone on --bg: the palette resolves no such token',
    ]);
  });

  /**
   * A value that is not `#rrggbb` reaches the arithmetic as a `NaN`, and
   * `NaN` compares false, so the pair passes vacuously rather than loudly.
   * `salvage.test.ts` takes three- and eight-digit hex from `theme.rs`, so
   * one can arrive here and be reported as fine.
   */
  it('finds a pair whose value it cannot measure', () => {
    expect(
      unreadable([['--bg', '--bg', TEXT]], { '--bg': 'rgba(15,49,30,.5)' }),
      'a value carrying an alpha channel',
    ).toEqual(['--bg on --bg: not a pair of #rrggbb values']);
  });

  /**
   * The guard that keeps the table from going stale the moment the sheet
   * changes: a colour arriving in the palette fails here until someone says
   * where it is drawn, rather than going unchecked.
   */
  it('pairs every token the palette resolves', () => {
    const paired = new Set(DRAWN.flatMap(([foreground, ground]) => [foreground, ground]));
    expect(
      Object.keys(rootTokens(null)).filter((token) => !paired.has(token)),
      'a palette token with no pair saying where it is drawn',
    ).toEqual([...NOT_INK]);
  });

  it('draws every pair it names at or above its floor', () => {
    expect(unreadable(DRAWN, rootTokens(null))).toEqual([]);
  });
});
