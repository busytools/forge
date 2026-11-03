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

/**
 * The reported phone, and the 320px floor this check holds the row to. The
 * client sets no minimum window width, so 320 is the smallest common Android
 * phone's CSS width rather than a bound the app enforces.
 */
const PHONES = [390, 320];
/** The finger floor `.mic`, `.voice`, `.stop`, `.micb` and `.send` take under a coarse pointer. */
const FLOOR = 44;
/** The controls on the row while a turn runs: mic, voice, stop, and the send a draft adds. */
const CONTROL_COUNTS = [3, 4];

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

/** The four sides of a padding shorthand, its two-value form expanded. */
function sides(declared: string): { top: number; right: number; bottom: number; left: number } {
  const text = /padding:\s*([^;]+)/.exec(declared)?.[1] ?? '';
  const [first, second, third, fourth] = text.trim().split(/\s+/).map(Number.parseFloat);
  if (first === undefined) throw new Error(`padding is empty in ${declared}`);
  if (second !== undefined && third === undefined && fourth === undefined) {
    return { top: first, right: second, bottom: first, left: second };
  }
  if (second !== undefined && third !== undefined && fourth !== undefined) {
    return { top: first, right: second, bottom: third, left: fourth };
  }
  throw new Error(`padding is not a shorthand in ${declared}`);
}

/** The declarations of every `(pointer: coarse)` block, joined. */
function coarse(): string {
  return [...SHEET.matchAll(/@media \(pointer: coarse\) \{\n([\s\S]*?)\n\}/g)]
    .map((match) => match[1])
    .join('\n');
}

/**
 * The width the card's readings need at the ladder step a container of
 * `container` fires.
 *
 * The numbers are the sheet's ladder comment: fit points measured in the real
 * engine by two independent rigs agreeing within 0.2px. Restated here so a
 * change on either side of the comparison is visible, and the sheet's own
 * steps are checked against them before the arithmetic runs.
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

    const media = coarse();
    expect(media, 'the mic lost the finger target it takes with the row').toMatch(
      /\.mic, \.voice, \.send, \.stop \{ min-height: 44px; min-width: 44px; \}/,
    );
    expect(media, 'the way out lost its 44px target').toMatch(
      /\.tc \.x::after \{[^}]*width: 44px; height: 44px;/,
    );
    expect(media, 'the way out grows right, into the mic it shares the row with').toMatch(
      /\.takeline \.tc \.x::after,[^{]*\{[^}]*right: 0;/,
    );

    const rowGap = pixels(rule('.line'), 'gap');
    const ctlGap = pixels(rule('.ctls'), 'gap');
    const pad = sides(rule('.surf .slot > .line'));
    const border = pixels(rule('.surf'), 'border');

    for (const phone of PHONES) {
      for (const controls of CONTROL_COUNTS) {
        const surface = phone - 2 * border;
        const controlsWidth = controls * FLOOR + (controls - 1) * ctlGap;
        const card = surface - pad.left - pad.right - rowGap - controlsWidth;
        // The nearest container the ladder measures is the card when the
        // sheet makes one, and the surface it sits in when it does not.
        const container = /container-type:\s*inline-size/.test(rule('.tc')) ? card : surface;
        const needs = readingsWidth(container);
        expect(
          needs,
          `${phone}px with ${controls} controls: the readings run out of the ${card}px card ` +
            'and over the mic beside it',
        ).toBeLessThanOrEqual(card + rowGap);
      }
    }
  });

  /**
   * **The card is the ladder's container in every mount, not just the
   * composer's.** The dock's words row, the takeover bar's feedback row, the
   * review and document word lines all draw the same card; the review and
   * document rows are the `.line` shape the first case already covers, and
   * the other two measure their own rows here. The intended consequence at a
   * phone: a transcribing card narrower than the 330px count step drops its
   * "N of M" count rather than keeping a reading it has no room for, and the
   * 430px mounts, which have the room, keep it.
   */
  it('measures the dock and the bar cards by the card, not the surface', () => {
    expect(
      rule('.tc'),
      'the container is scoped to one mount, so the others measure the surface again',
    ).toContain('container-type: inline-size');
    expect(coarse(), 'the dock and bar mic lost its finger floor').toMatch(
      /\.micb \{ min-height: 44px; min-width: 44px; \}/,
    );
    expect(SHEET, 'the count is no longer the reading the 330px step takes').toMatch(
      /@container \(max-width: 330px\) \{ \.tc \.ready, \.tc \.count \{ display: none; \} \}/,
    );

    const border = pixels(rule('.surf'), 'border');
    const dockPad = sides(rule('.dock'));
    const dockGap = pixels(rule('.dock .qrow'), 'gap');
    const barPad = sides(rule('.tb-feedback'));
    const barGap = pixels(rule('.tb-feedback'), 'gap');
    // The dock is a lane of the surface; the takeover bar spans the window.
    const cards = (phone: number): { dock: number; bar: number } => ({
      dock: phone - 2 * border - dockPad.left - dockPad.right - dockGap - FLOOR,
      bar: phone - barPad.left - barPad.right - barGap - FLOOR,
    });

    for (const phone of PHONES) {
      const drawn = cards(phone);
      for (const [where, card] of [
        ['dock', drawn.dock],
        ['bar', drawn.bar],
      ] as const) {
        expect(
          readingsWidth(card),
          `${phone}px ${where}: the readings overflow the ${card}px card`,
        ).toBeLessThanOrEqual(card);
        expect(
          card,
          `${phone}px ${where}: the count step does not fire on a card this narrow`,
        ).toBeLessThanOrEqual(330);
      }
    }

    const wide = cards(430);
    for (const [where, card] of [
      ['dock', wide.dock],
      ['bar', wide.bar],
    ] as const) {
      expect(card, `430px ${where}: the count leaves a card that has room for it`).toBeGreaterThan(
        330,
      );
    }
  });
});
