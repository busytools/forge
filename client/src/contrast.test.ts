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

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { rootTokens } from './theme';

/** WCAG 2.1 AA floors: 4.5:1 for text, 3:1 for a mark that is not text. */
const TEXT = 4.5;
const MARK = 3;

/**
 * The band the page's prose sits in, above WCAG's own floor for the pair.
 *
 * Both ends are this project's, because the standard has nothing to say about
 * text drawn too bright. The reference is the view this client replaced, which
 * drew body text at 6.10:1 (Ghostty `GitHub Dark`: fg #8b949e on #101216) and
 * nothing near white. The page's ground is darker and its lower tiers are
 * AA-floored, so it cannot follow that view down that far: prose has to stay
 * clearly above `--muted` (7.01:1), which puts this tier between roughly 10
 * and 14.
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

/** The sheet's spelling of a token, which is what a pair's entry names. */
const TOKEN = /^--[a-z0-9-]+$/;

/** And its spelling of a two-token lift: `color-mix(in srgb, var(--a) p%, var(--b))`. */
const MIX = /^color-mix\(in srgb, var\((--[a-z0-9-]+)\) (\d+)%, var\((--[a-z0-9-]+)\)\)$/;

/** An entry resolved to a `#rrggbb` value, or the reason it could not be. */
type Resolved = { ink: string } | { why: string };

/**
 * The colour an entry names: a token, or one of the two-token `color-mix`
 * expressions the sheet writes.
 *
 * **Reading the mix is what keeps the floor over a derived colour.** A pair
 * naming `color-mix(...)` used to arrive here as a value the arithmetic could
 * not run on, and `NaN` compares false, so it passed vacuously; resolving it
 * the way the browser does is what makes the command box's bright colours a
 * guarded pair rather than a comment.
 */
function inkOf(entry: string, palette: Readonly<Record<string, string>>): Resolved {
  if (TOKEN.test(entry)) {
    const value = palette[entry];
    return value === undefined ? { why: 'the palette resolves no such token' } : { ink: value };
  }
  const mix = MIX.exec(entry);
  if (mix === null) return { why: 'not a pair of #rrggbb values' };
  const [, left = '', percent = '0', right = ''] = mix;
  const a = palette[left];
  const b = palette[right];
  if (a === undefined || b === undefined) return { why: 'the palette resolves no such token' };
  const mixed = blend(a, b, Number(percent));
  return mixed === null ? { why: 'not a pair of #rrggbb values' } : { ink: mixed };
}

/**
 * `color-mix(in srgb, a percent%, b)` as a hex, or `null` when either side is
 * not a value this file can blend - plain sRGB interpolation, which is what
 * the browser does with two opaque colours.
 */
function blend(a: string, b: string, percent: number): string | null {
  if (!OPAQUE.test(a) || !OPAQUE.test(b)) return null;
  const p = percent / 100;
  const part = (at: number): string => {
    const left = Number.parseInt(a.slice(at, at + 2), 16);
    const right = Number.parseInt(b.slice(at, at + 2), 16);
    return Math.round(left * p + right * (1 - p))
      .toString(16)
      .padStart(2, '0');
  };
  return `#${part(1)}${part(3)}${part(5)}`;
}

/**
 * What a failure should say: the two tokens and both ratios.
 *
 * A pair outside its band is reported at whichever end it left: under the
 * floor it is unreadable, over the ceiling it is the glare the palette was
 * corrected for. A pair the arithmetic cannot be run on is reported rather
 * than measured, which is the whole of what keeps a `NaN` out: `NaN < floor`
 * is false, so anything reaching `contrast` unmeasured would read as fine.
 * Both halves are real - a token renamed in `theme.ts` leaves the table
 * stale, and a value outside the `#rrggbb` spelling (`#abc`, an eight-digit
 * hex, `rgba(...)`) is not a `#rrggbb` pair.
 */
function outOfBand(pairs: readonly Pair[], palette: Readonly<Record<string, string>>): string[] {
  const outside: string[] = [];
  for (const [foreground, ground, floor, ceiling] of pairs) {
    const fg = inkOf(foreground, palette);
    const bg = inkOf(ground, palette);
    if ('why' in fg) {
      outside.push(`${foreground} on ${ground}: ${fg.why}`);
      continue;
    }
    if ('why' in bg) {
      outside.push(`${foreground} on ${ground}: ${bg.why}`);
      continue;
    }
    if (!OPAQUE.test(fg.ink) || !OPAQUE.test(bg.ink)) {
      outside.push(`${foreground} on ${ground}: not a pair of #rrggbb values`);
      continue;
    }
    const measured = contrast(fg.ink, bg.ink);
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
 * A control's boundary has two neighbours, and the pair that matters is the
 * tighter one: the fill it encloses rather than the page outside it. The
 * connect screen's field and its button draw `--ctl` over `--s1` and `--s2`,
 * at 3.25 and 3.02 - 3.02 being the tightest margin the table carries, and the
 * pair to watch if `--ctl` or a raised ground moves.
 *
 * `--violet`'s rule is the pile card's own: `.m .src.forge` in `Queue.svelte`
 * paints it on the card's raised ground, and it is paired there so the token
 * cannot sit in the palette with no answer for where a surface would put it.
 *
 * The one ceiling is on the row the prose is read from: every other row that
 * draws `--text` is the same token on a raised ground, and always measures
 * below it.
 */
/**
 * The bright weight of an ANSI colour, as the sheet writes it: the token
 * lifted toward the page's prose tone. Held as the expression rather than a
 * number, so a lift that moves in the sheet is measured where it moved.
 */
const LIFTED = (token: string): string => `color-mix(in srgb, var(${token}) 70%, var(--text))`;

/**
 * The composite `.ansi-dim` paints: the token at the sheet's own opacity over
 * the box's ground. `opacity: .7` on text composites the glyph over what is
 * behind it, which is exactly this mix - so the values the attribute costs are
 * the ones the rows below measure.
 */
const DIMMED = (token: string): string => `color-mix(in srgb, var(${token}) 70%, var(--bg))`;

const DRAWN: readonly Pair[] = [
  ['--text', '--bg', PROSE_FLOOR, PROSE_CEILING],
  ['--text', '--s1', TEXT],
  ['--text', '--s2', TEXT],
  // The walked pile card's own ground: its words draw on `--s3`.
  ['--text', '--s3', TEXT],
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
  // The connect screen's two controls, each boundary read against the fill it
  // encloses rather than the page - `Connect.svelte`'s `.field :global(input)`
  // on `--s1` and its `button` on `--s2`.
  ['--ctl', '--s1', MARK],
  ['--ctl', '--s2', MARK],
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
  // The command output box's own colours (`.ansi-*` in `web.css`, read off the
  // escapes a command wrote by `chat/ansi.ts`), on the ground the box draws on.
  // The normal weight is the token itself - the eight rows above already carry
  // most of them - and the bright weight is the token lifted toward the page's
  // prose tone, written here exactly as the sheet writes it so the lift is
  // measured rather than trusted.
  ['--violet', '--bg', TEXT],
  [LIFTED('--dim'), '--bg', TEXT],
  [LIFTED('--bad'), '--bg', TEXT],
  [LIFTED('--ok'), '--bg', TEXT],
  [LIFTED('--warn'), '--bg', TEXT],
  [LIFTED('--blue'), '--bg', TEXT],
  [LIFTED('--violet'), '--bg', TEXT],
  [LIFTED('--teal'), '--bg', TEXT],
  // **`\e[2m` is the command's own attribute and draws dim.** A command that
  // asked for de-emphasis gets it, so these composites sit under the text
  // floor - and the floors are the values measured when the swap landed,
  // rounded down a tenth, so a heavier opacity is a red test rather than a
  // silent drift. Measured: dim 3.03, red 3.37, blue 4.04, yellow 4.45,
  // magenta 4.49, green 5.21, cyan 5.28, white 6.65.
  [DIMMED('--dim'), '--bg', 3.0],
  [DIMMED('--bad'), '--bg', 3.3],
  [DIMMED('--blue'), '--bg', 4.0],
  [DIMMED('--warn'), '--bg', 4.4],
  [DIMMED('--violet'), '--bg', 4.4],
  [DIMMED('--ok'), '--bg', 5.2],
  [DIMMED('--teal'), '--bg', 5.2],
  [DIMMED('--text'), '--bg', 6.6],
];

/**
 * The palette's one token with no pair. `--line` is the hairline: section
 * rules, code frames and popover dividers. Nothing is drawn on it, and a
 * separator is not information WCAG requires to be perceived - the single
 * rule that inks it, `.sess .facts .sep`, is a middot at 1.42:1.
 *
 * A border that identifies a control is a different job: it takes the 3:1 of
 * 1.4.11, and the connect screen's field and its button carry it in `--ctl`,
 * over `--s1` and `--s2` - the two rows above. Which token a rule NAMES is not
 * this file's to check, so those two rules are pinned where they are, in
 * `connect/controls.test.ts`; the controls still drawing `--line` over a
 * ground that does not separate them are #2306's sweep. Named rather than left
 * out, so a token arriving without a pair still fails.
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

  /**
   * **The mix path's own control**, separate from the token one: a lift that
   * lands too close to its ground must be reported exactly as a token is.
   * Without this, the new entry form could resolve to something that never
   * reaches `contrast` and every bright row would pass vacuously.
   */
  it('finds a lifted pair too close to its ground', () => {
    expect(
      outOfBand(
        [[`color-mix(in srgb, var(--bad) 20%, var(--bg))`, '--bg', TEXT]],
        rootTokens(null),
      ),
      'a colour lifted the wrong way on the box ground',
    ).toHaveLength(1);
  });

  /**
   * **The table's derived rows are the sheet's own text.** A lift copied here
   * would keep measuring an expression the sheet had stopped drawing - a
   * percentage changed in `web.css` would leave the guard green over a colour
   * nobody draws - so every derived row is asserted against the sheet
   * verbatim, and the dim composites against the opacity the sheet sets.
   */
  it('measures the lifts and the dim opacity the sheet actually draws', () => {
    const sheet = readFileSync(new URL('./assets/web.css', import.meta.url), 'utf8');
    for (const token of ['--dim', '--bad', '--ok', '--warn', '--blue', '--violet', '--teal']) {
      expect(sheet, `the sheet draws the bright weight for ${token}`).toContain(
        `color: ${LIFTED(token)}`,
      );
    }
    expect(sheet, 'and the dim composite is that sheet value').toContain(
      '.ansi-dim { opacity: .7; }',
    );
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
   * A palette may spell a hex as three or eight digits, so one can arrive
   * here and be reported as fine.
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
    ).toEqual([]);
  });

  it('draws every pair it names inside its band', () => {
    expect(outOfBand(DRAWN, rootTokens(null))).toEqual([]);
  });
});
