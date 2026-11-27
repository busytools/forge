import { describe, expect, it } from 'vitest';

import { fold, type Unit, type WorkRow } from './units';

/**
 * Where a stop-hook row lands, against the frames that followed it.
 *
 * **The frame order is the drawn order.** A silent Stop hook reports after the
 * assistant stops and the turn then carries on - in the captured seat
 * (`8768c065-9b72-41de-ba1a-835efe8c7a93`) one turn carries 1,379 frames after
 * its silent stop, and issue #3092 is the row that stayed below all of them.
 * A row drawn from a frame is owed the place its frame has: the hook row may
 * not sit under work that arrived after it.
 */

const use = (id: string): unknown => ({ type: 'tool_use', id, name: 'Read', input: {} });

/** One assistant frame carrying a single call. */
const call = (id: string): unknown => ({
  type: 'assistant',
  uuid: `a-${id}`,
  message: {
    id: `m-${id}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [use(id)],
  },
});

/** A message the reader sent, as the CLI carried it back into the turn. */
const typed = (text: string): unknown => ({
  type: 'user',
  uuid: `u-${text}`,
  message: { role: 'user', content: [{ type: 'text', text }] },
});

/**
 * The silent batch every stop of the reported seat carried: three plugin hooks,
 * no output, no error, no context (`hookCount: 3`, issue #3054).
 */
const silentStop = (): unknown => ({
  type: 'system',
  subtype: 'stop_hook_summary',
  hookCount: 3,
  hookInfos: [
    { command: 'echo fixture-stop-hook-1', durationMs: 3 },
    { command: 'echo fixture-stop-hook-2', durationMs: 4 },
    { command: 'echo fixture-stop-hook-3', durationMs: 5 },
  ],
  hookErrors: [],
  hookAdditionalContext: [],
  preventedContinuation: false,
  stopReason: '',
  hasOutput: false,
  uuid: 'stop-1',
});

/**
 * Every row a fold draws, in order: a work unit contributes its rows, and
 * every other unit is a row of its own. The hook chip is a work row, inside
 * the run that closes at the stop that drew it; the reader's message is a
 * unit of its own.
 */
const drawn = (units: Unit[]): string[] =>
  units.flatMap((unit) => (unit.kind === 'leaves' ? unit.rows.map((row) => row.tag) : [unit.kind]));

/** Every hook chip a fold drew, which is the turn's one at most. */
const feetOf = (units: Unit[]): Extract<WorkRow, { tag: 'hookfoot' }>[] =>
  units.flatMap((unit) =>
    unit.kind === 'leaves'
      ? unit.rows.filter(
          (row): row is Extract<WorkRow, { tag: 'hookfoot' }> => row.tag === 'hookfoot',
        )
      : [],
  );

/** Where the stop hook's own row sits, which is the only row it may draw. */
const hookAt = (rows: string[]): number =>
  rows.findIndex((tag) => tag === 'hookfoot' || tag === 'hookbatch');

describe('a stop-hook row and the frames after it', () => {
  it('draws a message that arrived after a silent stop below the hook row', () => {
    const rows = drawn(fold([typed('start'), silentStop(), typed('after the stop')]));
    const hook = hookAt(rows);
    const later = rows.lastIndexOf('user');
    expect(hook, `the stop hook drew a row, in ${JSON.stringify(rows)}`).toBeGreaterThanOrEqual(0);
    expect(later, `the message drew a row, in ${JSON.stringify(rows)}`).toBeGreaterThanOrEqual(0);
    expect(
      later,
      `the later message below the hook row, in ${JSON.stringify(rows)}`,
    ).toBeGreaterThan(hook);
  });

  it('draws a call that arrived after a silent stop below the hook row', () => {
    const rows = drawn(fold([call('before'), silentStop(), call('after')]));
    const hook = hookAt(rows);
    const later = rows.lastIndexOf('call');
    expect(hook, `the stop hook drew a row, in ${JSON.stringify(rows)}`).toBeGreaterThanOrEqual(0);
    expect(later, `the call drew a row, in ${JSON.stringify(rows)}`).toBeGreaterThanOrEqual(0);
    expect(later, `the later call below the hook row, in ${JSON.stringify(rows)}`).toBeGreaterThan(
      hook,
    );
  });

  it('keeps one chip across stops separated by work', () => {
    const units = fold([call('one'), silentStop(), call('two'), silentStop(), call('three')]);
    const feet = feetOf(units);
    expect(feet.length, `one chip, in ${JSON.stringify(drawn(units))}`).toBe(1);
    expect(feet[0]?.batches.length, 'both stops counted in it').toBe(2);
    expect(feet[0]?.actions, 'and every hook of both').toBe(6);
    expect(drawn(units), 'the chip stays where the first stop fired').toEqual([
      'call',
      'hookfoot',
      'call',
      'call',
    ]);
  });

  it('keeps one chip across consecutive stops after a call', () => {
    const units = fold([call('one'), silentStop(), silentStop()]);
    const feet = feetOf(units);
    expect(feet.length, `one chip, in ${JSON.stringify(drawn(units))}`).toBe(1);
    expect(feet[0]?.batches.length).toBe(2);
    expect(feet[0]?.actions).toBe(6);
    expect(drawn(units), 'the work above the chip is kept').toEqual(['call', 'hookfoot']);
  });
});
