import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The band's motion at the open, pinned to the sheet.
 *
 * **The defect this closes.** The load cycle opened on its own first frame -
 * the rest colour - while a focused band was already showing the accent, so
 * the frame after the gate dropped the border there in one frame: the open
 * read as a snap rather than a breath. The fix is a shape rather than
 * a number: the rule's base eases the accent in, and the cycle's clock waits
 * exactly that arrival out, so its first frame is the colour already on
 * screen. Reverting either half restores the snap, which is what these pin.
 *
 * **Read off the sheet, not the DOM.** Each assertion is a declaration or a
 * keyframe, and jsdom neither animates nor lays out. The drawn track - the
 * colour sampled per frame across a press, focused and unfocused - is
 * measured in the pull request; this is the shape kept from drifting.
 *
 * The same file pins the second half of that flinch: the band's own separator
 * rule took the hidden status line for a lane, so the gate that inserts it
 * grew the band a pixel and drew a hairline at the same instant.
 */
const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = SHEET.indexOf(`${selector} {`);
  expect(at, `the sheet has no rule for ${selector}`).toBeGreaterThan(-1);
  return SHEET.slice(at, SHEET.indexOf('}', at));
}

describe("the band's open", () => {
  it('lands the accent before the cycle starts breathing', () => {
    const load = rule('.surf.load');

    const transition = /(?:^|[;{]\s*)transition:\s*([^;]+)/.exec(load)?.[1]?.trim() ?? '';
    const animation = /(?:^|[;{]\s*)animation:\s*([^;]+)/.exec(load)?.[1]?.trim() ?? '';

    // `transition: border-color <clock> <easing>` and
    // `animation: <name> <duration> <easing> <delay> <count>`.
    const arrival = transition.split(/\s+/)[1];
    const wait = animation.split(/\s+/)[3];
    expect(
      wait,
      'the cycle does not wait the arrival out, so it starts on its own first frame instead of the colour on screen - the snap this shape removed',
    ).toBe(arrival);

    expect(
      /--arrive:\s*([^;]+);/.exec(load)?.[1]?.trim(),
      'the arrival clock is not a duration, so there is nothing to land before the cycle starts',
    ).toMatch(/^\d+(\.\d+)?(ms|s)$/);

    const first = /0%,\s*100%\s*\{ border-color: ([^;]+);/.exec(SHEET)?.[1]?.trim();
    const base = /(?:^|[;{]\s*)border-color:\s*([^;]+)/.exec(load)?.[1]?.trim();
    expect(
      first,
      'the cycle opens on a colour other than the one the arrival lands on, which is the frame that reads as a snap',
    ).toBe(base);

    expect(
      SHEET,
      'the load band lost its reduced-motion path: the state is the colour alone for a reader who asked for less motion',
    ).toMatch(/@media \(prefers-reduced-motion: reduce\) \{\s*\.surf\.load \{ animation: none;/);
  });

  it('draws no separator of its own, and keeps the hidden status line out of the flow', () => {
    // **The card is one bar, and its first row draws its own hairline.** A
    // `* + *` separator returning here draws a second line at the live row's
    // foot, and a status span left in the flow grows the band a pixel at the
    // open - the same flinch this pair has always caught, restated for the
    // card.
    expect(
      /\.surf > [^{]* \+ [^{]*\{ border-top:/.test(SHEET),
      "the surface draws a separator between children again, on top of the live row's own hairline",
    ).toBe(false);
    expect(rule('.live-row'), 'the live row lost the line under itself').toContain(
      'border-bottom: 1px solid var(--line)',
    );
    expect(
      rule('.comp .sr'),
      'the status line is back in the flow, where it grows the band at the open',
    ).toContain('position: absolute');
  });

  it('keeps the live row one line, with the chips collapsing rather than wrapping', () => {
    // The ragged wrap this replaced grew the band to three rows between 780
    // and 860px (Ved, in the #2159 look). `wrap` returning here is that band
    // coming back; a figure allowed to break is the same failure inside a
    // token pair - `18.4k` on one line and its arrow on the next.
    expect(rule('.live-row'), 'the live row wraps again').toContain('flex-wrap: nowrap');
    expect(rule('.live-row .live-fig'), 'a turn figure is allowed to break its own line').toContain(
      'white-space: nowrap',
    );
  });
});
