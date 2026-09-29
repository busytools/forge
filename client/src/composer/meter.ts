/**
 * The dictation meter, drawn from the take's own readings.
 *
 * The two scales are the shipped ones: the height is `Take::height` from
 * `crates/forge-server/src/composer.rs`, the steps the mockup draws the same
 * envelope at, and the two thresholds are its own colour ramp.
 */

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
