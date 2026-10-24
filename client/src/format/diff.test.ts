import { describe, expect, it } from 'vitest';

import { lineDiff, pairedTints, wordOps, wordSpans } from './diff';

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

describe('the line diff', () => {
  it('keeps every unchanged line as context around one changed line', () => {
    const lines = lineDiff('one\ntwo\nthree\nfour', 'one\nTWO\nthree\nfour');

    expect(lines.map((line) => line.kind)).toEqual(['ctx', 'del', 'add', 'ctx', 'ctx']);
    expect(lines[1]?.text).toBe('two');
    expect(lines[2]?.text).toBe('TWO');
  });

  it('keeps an unchanged line between two changes as context', () => {
    const lines = lineDiff('one\ntwo\nthree', 'ONE\ntwo\nTHREE');

    expect(lines.map((line) => line.kind)).toEqual(['del', 'add', 'ctx', 'del', 'add']);
    expect(lines[2]?.text).toBe('two');
  });

  it('does not turn a trailing newline into an empty changed line', () => {
    const lines = lineDiff('one\ntwo\n', 'one\nTWO\n');

    expect(lines.map((line) => line.kind)).toEqual(['ctx', 'del', 'add']);
  });

  /**
   * **Every line of each side survives, once, on its own side.** The walk is
   * hand-rolled, so a dropped or duplicated line in the LCS path would pass
   * the shape assertions above and render a wrong diff.
   */
  it('reconstructs each side from its own rows', () => {
    const cases: [string, string][] = [
      ['one\ntwo\nthree', 'one\nTWO\nthree'],
      ['a\nb\nc\nd', 'b\nd'],
      ['x\ny', 'x\nnew\ny'],
      ['first\nlast\n', 'first\nlast\n'],
    ];
    for (const [oldText, newText] of cases) {
      const lines = lineDiff(oldText, newText);
      const oldSide = lines.filter((line) => line.kind !== 'add').map((line) => line.text);
      const newSide = lines.filter((line) => line.kind !== 'del').map((line) => line.text);
      expect(oldSide.join('\n'), `the old side of ${JSON.stringify(oldText)}`).toBe(
        oldText.replace(/\n$/, ''),
      );
      expect(newSide.join('\n'), `the new side of ${JSON.stringify(newText)}`).toBe(
        newText.replace(/\n$/, ''),
      );
    }
  });

  /**
   * **Past the cap the middle is one removed run and one added run.** The
   * table is the renderer's cost, so a replacement large enough to need it
   * answers the whole middle at once rather than walking it.
   */
  it('answers one removed run and one added run past the cap', () => {
    // One line is identical on both sides in the middle, so a walk that did
    // not take the cap would find a context row; the cap answers the whole
    // middle as two runs, with none.
    const side = (mark: string, count: number, sharedAt: number): string =>
      Array.from({ length: count }, (_, at) => (at === sharedAt ? 'shared' : `${mark}${at}`)).join(
        '\n',
      );
    const lines = lineDiff(side('a', 600, 300), side('b', 600, 300));

    expect(lines.length, 'every line of both sides is present').toBe(1200);
    expect(
      lines.some((line) => line.kind === 'ctx'),
      'no walk past the cap',
    ).toBe(false);
  });
});

describe('the word tint spans', () => {
  /**
   * **A space between two changed words is part of the change.** Tinting each
   * word on its own left the space between them untinted, a highlight with
   * holes; the run either side of a space is one span.
   */
  it('tints the spaces between changed words as one run', () => {
    const spans = wordSpans('foo  bar', 'FOO  BAR');

    expect(spans.old, 'the whole changed run, spaces included').toEqual([{ start: 0, end: 8 }]);
    expect(spans.new).toEqual([{ start: 0, end: 8 }]);
  });

  /** Punctuation between two changed words is part of the run too. */
  it('tints shared punctuation between changed words', () => {
    const spans = wordSpans('alpha, beta', 'ALPHA, BETA');

    expect(spans.old, 'the comma does not break the highlight').toEqual([{ start: 0, end: 11 }]);
    expect(spans.new).toEqual([{ start: 0, end: 11 }]);
  });

  /** A shared word between two changes is not part of either one. */
  it('keeps two changes apart when a shared word stands between them', () => {
    const spans = wordSpans('alpha keep beta', 'ALPHA keep BETA');

    expect(spans.old).toEqual([
      { start: 0, end: 5 },
      { start: 11, end: 15 },
    ]);
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
