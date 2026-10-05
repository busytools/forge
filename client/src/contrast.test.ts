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
 * each other, at the WCAG ratio for what the sheet draws them as, plus the one
 * ceiling this project sets for its own prose. A colour
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

/**
 * The band the page's prose sits in, above WCAG's own floor for the pair.
 *
 * Both ends are this project's, because the standard has nothing to say about
 * text drawn too bright. The reference is the terminal, which draws body text
 * at 6.10:1 (Ghostty `GitHub Dark`: fg #8b949e on #101216) and nothing near
 * white. The page's ground is darker and its lower tiers are AA-floored, so it
 * cannot follow the terminal down that far: prose has to stay clearly above
 * `--muted` (7.01:1), which puts this tier between roughly 10 and 14.
 *
 * Both ends are enforced rather than described: 14 is where a later "the chat
 * looks dim" cannot put `--text` back to, since it measured 17.51:1, and 10 is
 * where a softer step stops reading as the brightest tier and starts
 * flattening into the one below it.
 */
const PROSE_FLOOR = 10;
const PROSE_CEILING = 14;

/**
 * A foreground token, the ground it is drawn on, the floor it takes, and - on
 * the token the page draws its prose in - the ceiling it stays under.
 */
type Pair = readonly [foreground: string, ground: string, floor: number, ceiling?: number];

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
 * A pair outside its band is reported at whichever end it left: under the
 * floor it is unreadable, over the ceiling it is the glare the palette was
 * corrected for. A pair the arithmetic cannot be run on is reported rather
 * than measured, which is the whole of what keeps a `NaN` out: `NaN < floor`
 * is false, so anything reaching `contrast` unmeasured would read as fine.
 * Both halves are real - a token renamed in `theme.ts` leaves the table
 * stale, and a value `salvage.test.ts` accepts from `theme.rs` (`#abc`, an
 * eight-digit hex, `rgba(...)`) is not a `#rrggbb` pair.
 */
function outOfBand(pairs: readonly Pair[], palette: Readonly<Record<string, string>>): string[] {
  const outside: string[] = [];
  for (const [foreground, ground, floor, ceiling] of pairs) {
    const fg = palette[foreground];
    const bg = palette[ground];
    if (fg === undefined || bg === undefined) {
      outside.push(`${foreground} on ${ground}: the palette resolves no such token`);
      continue;
    }
    if (!OPAQUE.test(fg) || !OPAQUE.test(bg)) {
      outside.push(`${foreground} on ${ground}: not a pair of #rrggbb values`);
      continue;
    }
    const measured = contrast(fg, bg);
    if (measured < floor) {
      outside.push(
        `${foreground} on ${ground} measures ${measured.toFixed(2)}:1, below the ${floor}:1 floor`,
      );
    }
    if (ceiling !== undefined && measured > ceiling) {
      outside.push(
        `${foreground} on ${ground} measures ${measured.toFixed(2)}:1, above the ${ceiling}:1 ceiling`,
      );
    }
  }
  return outside;
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
 * Every pair comes from a rule, in `assets/web.css` or in a component's own
 * style block, and names the token that rule paints. Where a
 * rule paints over a gradient or an alpha layer the row still has to name a
 * token, so its number reads higher than the ground's: `--blue` is
 * `.opt .ic.ed` in the dock's gradient, at 6.80-7.08 rather than 7.38;
 * `--hot` is `.dict .db` in the composer's, at 10.21-10.64 rather than 11.25;
 * `--ctl` is `.opt .box2` in the dock's gradient at 3.23-3.36 rather than
 * 3.50, and 3.10 on the selected row's own ground. Each clears its floor
 * where it lands.
 *
 * `--violet`'s rule is the pile card's own: `.m .src.forge` in `Queue.svelte`
 * paints it on the card's raised ground, and it is paired there so the token
 * cannot sit in the palette with no answer for where a surface would put it.
 *
 * The one ceiling is on the row the prose is read from: every other row that
 * draws `--text` is the same token on a raised ground, and always measures
 * below it.
 */
const DRAWN: readonly Pair[] = [
  ['--text', '--bg', PROSE_FLOOR, PROSE_CEILING],
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
  // The dictation panel's own mark: its device row is a raised control, and the
  // tag on a pin the walk could not find draws in this tone on that ground.
  ['--bad', '--s2', TEXT],
  ['--blue', '--bg', TEXT],
  ['--teal', '--bg', TEXT],
  ['--teal', '--s2', TEXT],
  // The pile card's own source chip: `.m .src.forge`, on the card's ground.
  ['--violet', '--s2', TEXT],
  ['--hot', '--bg', TEXT],
];

/**
 * The palette's one token with no pair. `--line` is the hairline: section
 * rules, code frames and popover dividers. Nothing is drawn on it, and a
 * separator is not information WCAG requires to be perceived - the single
 * rule that inks it, `.sess .facts .sep`, is a middot at 1.42:1.
 *
 * A border that identifies a control is a different job, and the sheet's own
 * controls have left this token: the dock's box, its checkbox and its notes
 * field all draw `--ctl` at rest, carried above at the 3:1 of 1.4.11. The
 * connect screen's address field and its button still draw `--line`, over
 * fills of 1.08:1 and 1.16:1 against the page, so neither fill separates from
 * it and the field's boundary is drawn by its border alone.
 * That screen is #1320 and owes its own drawing rather than a mirror here.
 * Named rather than left out, so a token arriving without a pair still fails.
 */
const NOT_INK = ['--line'];

/**
 * The one ground the TABLE names that carries no text pair: `--s3` is a
 * progress track there, and the only pair naming it is the bar fill. Named
 * rather than left out, so a ground whose text pair goes missing is a failure
 * and not a quiet exception.
 *
 * **What it does not scan**: a component's own style block. The walked pile
 * card draws its words and its source chips on `--s3`, and nothing here reads
 * that block - so this claims what the table names, not everything the sheet
 * draws.
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
    expect(outOfBand([UNREADABLE], rootTokens(null)), 'a pair one ground apart').toHaveLength(1);
  });

  /**
   * The ceiling's own control, and it is separate from the floor's: the two
   * are different ends of the band, so a check that can only report the low
   * one would pass this file while never measuring the glare it was added
   * for. A ceiling nothing can fail is a comment wearing an assertion.
   */
  it('finds a pair drawn past its ceiling', () => {
    expect(
      outOfBand([['--text', '--bg', TEXT, 12]], { '--text': '#eaeef6', '--bg': '#04050a' }),
      'prose against a ceiling under its own value',
    ).toHaveLength(1);
  });

  /**
   * The floor's control, on the prose row's own numbers: the branch is already
   * covered by the pair one ground apart, but a floor weakened to 0 would
   * leave every other test here green. This grey clears AA and still sits
   * under the floor, which is the flattening the band exists to stop.
   */
  it('finds prose drawn under its floor', () => {
    expect(
      outOfBand([['--text', '--bg', PROSE_FLOOR, PROSE_CEILING]], {
        '--text': '#a4afbd',
        '--bg': '#04050a',
      }),
      'a grey above AA and under the prose floor',
    ).toHaveLength(1);
  });

  /** A pair naming a token the palette does not resolve is a stale pair. */
  it('finds a pair naming a token the palette has lost', () => {
    expect(outOfBand([['--gone', '--bg', TEXT]], rootTokens(null)), 'a renamed token').toEqual([
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
      outOfBand([['--bg', '--bg', TEXT]], { '--bg': 'rgba(15,49,30,.5)' }),
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

  it('draws every pair it names inside its band', () => {
    expect(outOfBand(DRAWN, rootTokens(null))).toEqual([]);
  });
});
