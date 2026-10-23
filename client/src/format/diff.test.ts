import { describe, expect, it } from 'vitest';

import { pairedTints, wordOps } from './diff';

/** `count` words, each carrying `mark` so the two sides share nothing. */
const words = (count: number, mark: string): string =>
  Array.from({ length: count }, (_, at) => `${mark}${at}`).join(' ');

describe('the word diff', () => {
  /**
   * **Past the cap the line is one delete and one insert.** The quadratic
   * walk on a 441,094-character line is the renderer's cost, so the cap
   * answers the whole middle at once; a regression that removed it would
   * walk tens of thousands of words per frame.
   */
  it('answers one delete and one insert when the change is over the word cap', () => {
    const ops = wordOps(words(401, 'old'), words(401, 'new'));

    expect(
      ops.map((op) => op.kind),
      'no word-by-word walk past the cap',
    ).toEqual(['del', 'ins']);
  });

  /** Under the cap the same shape is still a delete and an insert, but the
   *  shared leading text is kept equal - the control for the cap. */
  it('keeps the shared words under the cap', () => {
    const ops = wordOps('the old value', 'the new value');

    expect(ops.map((op) => op.kind)).toEqual(['equal', 'del', 'ins', 'equal']);
    expect(
      ops
        .filter((op) => op.kind !== 'del')
        .map((op) => op.text)
        .join(''),
    ).toBe('the new value');
  });
});

describe('the pairing', () => {
  it('puts no tint on an unpaired removal or a surplus addition', () => {
    const tints = pairedTints([
      { kind: 'del', text: 'removed only' },
      { kind: 'ctx', text: 'context' },
    ]);

    expect(tints, 'an unpaired removal has no word tint').toEqual([[], []]);
  });
});
