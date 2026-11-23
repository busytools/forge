/**
 * How a page of turns becomes the virtualised list's items.
 *
 * **The property under test is that nothing is lost**: an ordinary turn is one
 * item drawn whole, and a turn too tall for one item is the same units and the
 * same rows, in the same order, cut into slices - which is what lets the list
 * draw only the slice a reader is looking at.
 */
import { describe, expect, it } from 'vitest';

import { chatRows, drawnRows, firstRowOf, partDraws, rowOfCall, rowOfKey, SPLIT_AT } from './rows';
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

  it('splits at the threshold and not a row earlier', () => {
    const whole = chatRows(turns(SPLIT_AT), unitsOf);
    expect(whole.length, 'exactly the threshold is one item').toBe(1);
    expect(whole[0]?.part, 'and it is drawn whole').toBeNull();

    const over = chatRows(turns(SPLIT_AT + 1), unitsOf);
    expect(over.length, 'a row over it is taken apart').toBeGreaterThan(1);
    expect(over[0]?.part?.first, 'the first item says so').toBe(true);
    expect(over.at(-1)?.part?.last, 'and the last').toBe(true);
    // **The last slice is the remainder**, which a slicer that dropped a tail
    // would not draw: the turn ends exactly one row past the threshold.
    const tail = over.at(-1)?.part?.units.at(-1);
    expect(tail?.kind).toBe('leaves');
    if (tail?.kind === 'leaves') expect(tail.rows.length, 'the tail is drawn').toBe(1);
    // **The head keeps both its names**: the turn's first item keeps the
    // turn's key and its run keeps the run's, so a live turn crossing the
    // split does not remount the block the reader is on.
    expect(over[0]?.key, "the first item keeps the turn's key").toBe('t0');
    expect(over[0]?.part?.units[0]?.key, "and the first slice keeps the run's").toBe('t0-run');
  });

  it('keeps every row and its order across the cut', () => {
    const page = turns(SPLIT_AT * 3);
    const rows = chatRows(page, unitsOf);
    // **The ordered keys, not a de-duplication of them**: a comparison against
    // the list's own set passes in any order, which is the property this test
    // exists to deny. The first slice keeps the run's key, and each later one
    // is named by the row it starts at.
    expect(drawn(rows), "the slices, in the run's order").toEqual(
      Array.from({ length: (SPLIT_AT * 3) / 30 }, (_, slice) =>
        slice === 0 ? 't0-run' : `t0-run~t0-run-${slice * 30}`,
      ),
    );
    const rowsDrawn = rows.reduce((total, row) => {
      const part = row.part;
      if (part === null) return total;
      return total + part.units.reduce((held, unit) => held + drawnRows(unit), 0);
    }, 0);
    expect(rowsDrawn, 'no row is dropped by the cut').toBe(SPLIT_AT * 3);
  });

  it('keeps the prose between two runs whole, on its own key', () => {
    // A run is cut through, but a unit between two runs is not: the prose
    // lands whole in a slice of its own, and the key it is found by is the
    // unit's own - the path `partDraws` and `rowOfKey` take for every unit
    // that is not a slice of a run.
    const prose: Unit = { kind: 'text', key: 'mid', text: 'between' };
    const units: Unit[] = [run('a', 100), prose, run('b', 100)];
    const rows = chatRows(turns(1), () => units, SPLIT_AT, 30);
    const at = rows.findIndex((row) => row.part?.units.some((unit) => unit.key === 'mid'));
    expect(at, 'the prose has an item of its own').toBeGreaterThan(0);
    expect(
      rows[at]?.part?.units.map((unit) => unit.kind),
      'and nothing else in it',
    ).toEqual(['text']);
    const part = rows[at]?.part;
    expect(part, 'the item is a slice').not.toBeNull();
    if (part === null || part === undefined) return;
    expect(partDraws(part, 't0', 't0:mid'), 'the unit key finds it').toBe(true);
    expect(rowOfKey(rows, 't0', 't0:mid'), 'and the list is sent to it').toBe(at);
    // The runs either side are sliced, so the prose sits between slices of
    // two different runs rather than inside one.
    expect(rows[0]?.part?.units[0]?.kind, 'a run leads').toBe('leaves');
    expect(rows.at(-1)?.part?.units.at(-1)?.kind, 'and another closes').toBe('leaves');
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
    // **A dispatch jump asks by the call's identity, not by a name spelled
    // from it.** The fold's own naming carries the occurrence, so a jump that
    // spelled the name from the id would stop matching the moment that naming
    // changed; the tool_use id is what the caller holds.
    expect(rowOfCall(rows, 't0', 'abc'), 'the call is found by its id').toBe(rows.length - 1);
    expect(rowOfCall(rows, 't0', 'not-a-call'), 'an unknown call lands on the turn').toBe(0);
  });
});
