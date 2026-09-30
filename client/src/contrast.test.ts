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
 * Every pair the palette has to carry: the foreground token, the ground under
 * it, and the floor that applies. This comment says what the floors mean and
 * where a number is not the page's; the table below is the list of what is
 * drawn and where, so neither has to count the other.
 *
 * The floor follows what the sheet draws the colour AS, not which token it
 * is: `TEXT` for anything read as words, `MARK` for a surface that carries
 * information without being text - a control's border, a bar fill. Nothing
 * here takes the 3:1 large-text floor, since the sheet's largest is
 * `.brand .word` at 22px and weight 650, neither 24px nor bold.
 *
 * Every pair but `--violet`'s comes from a rule, in `assets/web.css` or in a
 * component's own style block, and names the token that rule paints. Where a
 * rule paints over a gradient or an alpha layer the row still has to name a
 * token, so its number reads higher than the ground's: `--blue` is
 * `.opt .ic.ed` in the dock's gradient, at 6.80-7.08 rather than 7.38;
 * `--hot` is `.dict .db` in the composer's, at 10.21-10.64 rather than 11.25;
 * `--ctl` is `.opt .box2` in the dock's gradient at 3.23-3.36 rather than
 * 3.50, and 3.10 on the selected row's own ground. Each clears its floor
 * where it lands.
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
 * A border that identifies a control is a different job, and the sheet's own
 * controls have left this token: the dock's checkbox and notes field, the two
 * `.tog` chips and the account pill all draw `--ctl`, carried above at the
 * 3:1 of 1.4.11. The connect screen's address field and its button still draw
 * `--line`, over fills of 1.08:1 and 1.16:1 against the page, so neither fill
 * separates from it and the field's boundary is drawn by its border alone.
 * That screen is #1320 and owes its own drawing rather than a mirror here.
 * Named rather than left out, so a token arriving without a pair still fails.
 */
const NOT_INK = ['--line'];

/**
 * The one ground the sheet draws no text on: `--s3` is a progress track, and
 * the only pair naming it is the bar fill. Named rather than left out, so a
 * ground whose text pair goes missing is a failure and not a quiet exception.
 */
const NO_TEXT_GROUND = ['--s3'];

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

  /**
   * The guard above says a token is paired somewhere, which is not the same
   * as the pair that matters still being there: dropping the `--dim` row
   * leaves `--sel` paired by the control border, and this file goes green
   * while the ground the selected row draws its text on is no longer checked.
   */
  it('carries text on every ground the table names', () => {
    const grounds = new Set(DRAWN.map(([, ground]) => ground));
    const withText = new Set(
      DRAWN.filter(([, , floor]) => floor === TEXT).map(([, ground]) => ground),
    );
    expect(
      [...grounds].filter((ground) => !withText.has(ground)).sort(),
      'a ground that has lost its text pair',
    ).toEqual([...NO_TEXT_GROUND]);
  });

  it('draws every pair it names at or above its floor', () => {
    expect(unreadable(DRAWN, rootTokens(null))).toEqual([]);
  });
});
