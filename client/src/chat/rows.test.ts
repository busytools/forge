/**
 * How a page of turns becomes the virtualised list's items.
 *
 * **The property under test is that nothing is lost**: an ordinary turn is one
 * item drawn whole, and a turn too tall for one item is the same units and the
 * same rows, in the same order, cut into slices - which is what lets the list
 * draw only the slice a reader is looking at.
 */
import { describe, expect, it } from 'vitest';

import { chatRows, drawnRows, firstRowOf, partDraws, rowOfKey, SPLIT_AT } from './rows';
import type { Unit } from './units';

/** A run of `count` thoughts, which draws one row each. */
function run(key: string, count: number): Unit {
  return {
    kind: 'leaves',
    key,
    rows: Array.from({ length: count }, (_, at) => ({
      tag: 'thought' as const,
      key: `${key}-${at}`,
      text: `thought ${at}`,
    })),
  };
}

/** A turn the caller folds as it asks. */
function turns(...sizes: number[]): { key: string; size: number }[] {
  return sizes.map((size, at) => ({ key: `t${at}`, size }));
}

/** The units `chatRows` folds for a turn of `size` rows, as one run each. */
function unitsOf(turn: { key: string; size: number }): Unit[] | null {
  return turn.size <= 1 ? null : [run(`${turn.key}-run`, turn.size)];
}

/** Every unit of every item, in the order the list draws them. */
function drawn(rows: ReturnType<typeof chatRows<{ key: string; size: number }>>): string[] {
  return rows.flatMap((row) => (row.part === null ? [] : row.part.units.map((unit) => unit.key)));
}

describe('the items a page of turns becomes', () => {
  it('draws an ordinary turn whole', () => {
    const page = turns(2, 5);
    const rows = chatRows(page, unitsOf);
    expect(rows.map((row) => row.key)).toEqual(['t0', 't1']);
    expect(
      rows.every((row) => row.part === null),
      'neither turn is taken apart',
    ).toBe(true);
  });

  it('takes apart a turn over the split, and only that one', () => {
    const page = turns(5, SPLIT_AT + 1, 4);
    const rows = chatRows(page, unitsOf);
    const split = rows.filter((row) => row.turn.key === 't1');
    expect(split.length, 'the tall turn is more than one item').toBeGreaterThan(1);
    expect(
      rows
        .filter((row) => row.turn.key === 't0' || row.turn.key === 't2')
        .every((row) => row.part === null),
      'the short turns are untouched',
    ).toBe(true);
    expect(split[0]?.part?.first, 'the first item says so').toBe(true);
    expect(split.at(-1)?.part?.last, 'and the last').toBe(true);
    expect(split.filter((row) => row.part?.first === true).length, 'exactly one first item').toBe(
      1,
    );
    expect(split.filter((row) => row.part?.last === true).length, 'exactly one last item').toBe(1);
  });

  it('keeps every row and its order across the cut', () => {
    const page = turns(SPLIT_AT * 3);
    const rows = chatRows(page, unitsOf);
    const keys = drawn(rows);
    expect(keys, 'each slice of the run is drawn once').toEqual([...new Set(keys)]);
    const rowsDrawn = rows.reduce((total, row) => {
      const part = row.part;
      if (part === null) return total;
      return total + part.units.reduce((held, unit) => held + drawnRows(unit), 0);
    }, 0);
    expect(rowsDrawn, 'no row is dropped by the cut').toBe(SPLIT_AT * 3);
  });

  it('names each item by the unit it starts at, which appends cannot move', () => {
    const page = turns(SPLIT_AT * 2);
    const before = chatRows(page, unitsOf).map((row) => row.key);
    const longer = turns(SPLIT_AT * 2 + 5);
    const after = chatRows(longer, unitsOf).map((row) => row.key);
    expect(new Set(before).size, 'keys are unique').toBe(before.length);
    // Every item but the one the rows landed in keeps its name, so the list
    // does not remount what the reader is looking at when a frame arrives.
    expect(after.slice(0, before.length - 1)).toEqual(before.slice(0, before.length - 1));
  });

  it('lands a jump on the item that draws the row', () => {
    const page = turns(SPLIT_AT * 2);
    const rows = chatRows(page, unitsOf);
    const last = rows.at(-1);
    const part = last?.part;
    expect(part).not.toBeNull();
    const unit = part?.units.at(-1);
    expect(unit).toBeDefined();
    if (part === null || part === undefined || unit === undefined) return;
    const key = `t0:${unit.key}`;
    expect(partDraws(part, 't0', key), 'the part knows the row it draws').toBe(true);
    expect(rowOfKey(rows, 't0', key), 'and the list is sent to that item').toBe(rows.length - 1);
    expect(rowOfKey(rows, 't0', 't0:not-a-row'), 'an unknown row lands on the turn').toBe(0);
    expect(firstRowOf(rows, 't0'), 'the turn starts at its first item').toBe(0);
  });

  it('finds a call row inside a run', () => {
    const call: Unit = {
      kind: 'leaves',
      key: 'run',
      rows: [
        {
          tag: 'call',
          key: 'c-abc',
          leaf: {
            id: 'abc',
            row: { kind: 'browser' },
            name: 'Bash',
            title: 'Bash',
            command: 'ls',
            status: 'completed',
            backgrounded: false,
            note: null,
            stop: null,
            body: [],
            mutation: null,
            decision: null,
            forge: null,
            skill: null,
            image: null,
            imageNote: null,
          },
        },
      ],
    };
    const rows = chatRows(turns(1), () => [run('lead', 200), call], 1, 100);
    // The call lands in the second item, which is the one a dispatch jump
    // must reach: `call-` and the row's key is what the DOM draws.
    expect(rowOfKey(rows, 't0', 'call-c-abc')).toBe(rows.length - 1);
  });
});
