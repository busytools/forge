import { describe, expect, it } from 'vitest';

import { meterCells } from './meter';

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
