import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The take row at a phone width under a finger, where the way out sat on the
 * microphone (#2842).
 *
 * **Read off the sheet, because jsdom performs no layout.** What this computes
 * is the engine's own arithmetic for this row: the controls at their finger
 * floor, the card the field gives up beside them, and the width the card's
 * readings need at the ladder step its container fires. The rects themselves
 * were measured in the app at 390x844 with `pointer: coarse`, and the pull
 * request carries those numbers.
 */

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** The report's phone, and the narrowest one the client draws at. */
const PHONES = [390, 320];
/** The finger floor `.mic`, `.voice` and `.stop` take under a coarse pointer. */
const FLOOR = 44;
/** The controls on the row while a turn runs and a take is held: mic, voice, stop. */
const CONTROLS = 3;

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = SHEET.indexOf(`${selector} {`);
  expect(at, `the sheet has no rule for ${selector}`).toBeGreaterThan(-1);
  return SHEET.slice(at, SHEET.indexOf('}', at));
}

/** A declaration's length in pixels, from the rule's own text. */
function pixels(declared: string, property: string): number {
  const text = new RegExp(`(?:^|[;{])\\s*${property}:\\s*([\\d.]+)px`).exec(declared)?.[1];
  expect(text, `${property} is not a length in ${declared}`).toBeDefined();
  return Number(text);
}

/** The four sides of a padding shorthand. */
function sides(declared: string): [number, number, number, number] {
  const text = /padding:\s*([^;]+)/.exec(declared)?.[1] ?? '';
  const parts = text.trim().split(/\s+/).map(Number.parseFloat);
  expect(parts, `padding is not four lengths in ${declared}`).toHaveLength(4);
  const [top, right, bottom, left] = parts;
  if (top === undefined || right === undefined || bottom === undefined || left === undefined) {
    throw new Error(`padding is not four lengths in ${declared}`);
  }
  return [top, right, bottom, left];
}

/**
 * The width the card's readings need at the ladder step a container of
 * `container` fires.
 *
 * The card is 506px with every reading, 457 without the clock, 331 without the
 * graph, 267 without the tally, 196 without the frames, 124 without the bytes,
 * and 52.5 alone with the state and the way out; each step fires one below its
 * fit point, and the last floor is the card's chrome.
 */
const LADDER = [
  { step: 505, needs: 506 },
  { step: 456, needs: 457 },
  { step: 330, needs: 331 },
  { step: 266, needs: 267 },
  { step: 195, needs: 196 },
  { step: 124, needs: 124 },
] as const;

function readingsWidth(container: number): number {
  for (const row of LADDER) {
    if (container > row.step) return row.needs;
  }
  return 52.5;
}

describe('the take row at a phone width under a finger', () => {
  it('keeps the way out off the mic, with a finger target on each', () => {
    // The ladder this test computes from is the sheet's: a moved step or a
    // deleted arm would otherwise make the arithmetic describe a card that is
    // not on the page.
    const steps = [...SHEET.matchAll(/@container \(max-width: (\d+)px\) \{ \.tc /g)].map((match) =>
      Number(match[1]),
    );
    expect(steps, 'the card ladder moved off the widths this check computes from').toEqual(
      LADDER.map((row) => row.step),
    );

    const coarse = [...SHEET.matchAll(/@media \(pointer: coarse\) \{\n([\s\S]*?)\n\}/g)]
      .map((match) => match[1])
      .join('\n');
    expect(coarse, 'the mic lost the finger target it takes with the row').toMatch(
      /\.mic, \.voice, \.send, \.stop \{ min-height: 44px; min-width: 44px; \}/,
    );
    expect(coarse, 'the way out lost its 44px target').toMatch(
      /\.tc \.x::after \{[^}]*width: 44px; height: 44px;/,
    );
    expect(coarse, 'the way out grows right, into the mic it shares the row with').toMatch(
      /\.takeline \.tc \.x::after,[^{]*\{[^}]*right: 0;/,
    );

    const rowGap = pixels(rule('.line'), 'gap');
    const ctlGap = pixels(rule('.ctls'), 'gap');
    const [, padRight, , padLeft] = sides(rule('.surf .slot > .line'));
    const border = pixels(rule('.surf'), 'border');

    for (const phone of PHONES) {
      const surface = phone - 2 * border;
      const controls = CONTROLS * FLOOR + (CONTROLS - 1) * ctlGap;
      const card = surface - padLeft - padRight - rowGap - controls;
      // The nearest container the ladder measures is the card when the sheet
      // makes one, and the surface it sits in when it does not.
      const container = /container-type:\s*inline-size/.test(rule('.tc')) ? card : surface;
      const needs = readingsWidth(container);
      expect(
        needs,
        `${phone}px: the readings run out of the ${card}px card and over the mic beside it`,
      ).toBeLessThanOrEqual(card + rowGap);
    }
  });
});
