import { describe, expect, it } from 'vitest';

import { METER_CELLS } from '../wire/limits';
import { meterCells, meterWindow } from './meter';

/**
 * The meter is a history of readings rather than bars pulsing in place, so it
 * is drawn from the readings the take reported: one cell each, newest last,
 * graded by how loud that reading was.
 *
 * Both scales are the shipped ones - the height is `Take::height` from
 * `crates/forge-server/src/composer.rs`, and the two thresholds are the
 * mockup's own ramp.
 */
describe('the dictation meter', () => {
  it('gives each reading the height and the tone its level earns', () => {
    expect(meterCells([0, 0.4, 0.7, 1])).toEqual([
      { height: 12, tone: '' },
      { height: 46, tone: 'mid' },
      { height: 71, tone: 'hot' },
      { height: 96, tone: 'hot' },
    ]);
  });

  it('keeps the readings in the order they arrived, newest last', () => {
    const tones = meterCells([0.9, 0.1, 0.5]).map((cell) => cell.tone);
    expect(tones, 'the meter reads left to right in time order').toEqual(['hot', '', 'mid']);
  });
});

/**
 * The window the meter is drawn as: a fixed number of cells whatever the take
 * has reported so far.
 *
 * **Why it is fixed rather than the count of readings.** The bars share out
 * the track between them, so a window of three readings is three bars a third
 * of the track wide and a window of four is four bars a quarter of it: every
 * arriving reading would resize every bar and re-lay the whole graph out, all
 * the way through a take's first seconds. A fixed window holds the pitch still
 * and moves only the heights.
 *
 * The head is drawn as floor cells rather than left empty, which is the
 * terminal's own answer to the same thing: `forge-tui`'s window is a vector of
 * `METER_WIDTH` zeros that fills as readings arrive.
 */
describe("the meter's window", () => {
  it('draws the meter full whatever the take has reported, so bars do not resize', () => {
    expect(meterWindow([0.5]).length, 'a young take is drawn as a full window').toBe(METER_CELLS);
    const arrived = Array.from({ length: 9 }, (_, at) => at / 8);
    expect(meterWindow(arrived).length, 'the window is the same length once readings arrive').toBe(
      METER_CELLS,
    );
  });

  it('draws the readings it has at the newest end, floor cells before them', () => {
    const window = meterWindow([0, 0.4, 0.7, 1]);
    const head = window.slice(0, METER_CELLS - 4);
    expect(
      head.every((cell) => cell.height === 12 && cell.tone === ''),
      'the head is the floor',
    ).toBe(true);
    expect(window.slice(-4), 'the readings keep their own heights and tones').toEqual(
      meterCells([0, 0.4, 0.7, 1]),
    );
  });

  it('keeps the newest windowful when more readings arrive than it holds', () => {
    const many = Array.from({ length: METER_CELLS + 12 }, (_, at) =>
      at === METER_CELLS + 11 ? 1 : 0,
    );
    const window = meterWindow(many);
    expect(window.length, 'the window never grows past what it holds').toBe(METER_CELLS);
    expect(window.at(-1)?.tone, 'the newest reading is the one kept at the newest end').toBe('hot');
  });
});
