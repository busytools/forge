/**
 * The dictation meter, drawn from the take's own readings.
 *
 * The two scales are the shipped ones: the height is `Take::height` from
 * `crates/forge-server/src/composer.rs`, the steps the mockup draws the same
 * envelope at, and the two thresholds are its own colour ramp.
 */

import { METER_CELLS } from '../wire/limits';

/** One cell of the meter: a past reading, as tall and as loud as it was. */
export interface Cell {
  /** Percent of the meter's height, floor to ceiling. */
  height: number;
  /** The tone class the sheet colours it by, empty for the quietest step. */
  tone: '' | 'mid' | 'hot';
}

/** The shortest a cell is drawn: a reading is never nothing at all. */
const FLOOR_PERCENT = 12;

/** What the loudest reading leaves for the track's own padding. */
const HEADROOM_PERCENT = 4;

const HOT = 0.7;
const MID = 0.4;

/** One cell per reading, newest last, which is the order the meter reads in. */
export function meterCells(levels: number[]): Cell[] {
  return levels.map((level) => ({
    height: Math.round(FLOOR_PERCENT + level * (100 - FLOOR_PERCENT - HEADROOM_PERCENT)),
    tone: level >= HOT ? 'hot' : level >= MID ? 'mid' : '',
  }));
}

/**
 * The window the meter is drawn as: a full-length history with the readings
 * the take has reported at its newest end.
 *
 * The cells short of that are drawn as the floor rather than left out, because
 * the bars share out the track between them: a window of three readings would
 * be three bars a third of the track wide and the next reading would resize
 * all of them. A fixed length holds the pitch still and lets the heights be
 * the only thing that moves. `forge-tui`'s own meter is the same shape, a
 * vector of `METER_WIDTH` zeros that fills as readings arrive.
 */
export function meterWindow(levels: number[]): Cell[] {
  const readings = levels.slice(-METER_CELLS);
  const floor = Array.from({ length: METER_CELLS - readings.length }, () => 0);
  return meterCells([...floor, ...readings]);
}
