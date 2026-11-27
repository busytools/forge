import { describe, expect, it } from 'vitest';

import { fold, type Unit } from './units';

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
 * every other unit is a row of its own (the reader's message and the hook
 * chip are both units rather than work rows).
 */
const drawn = (units: Unit[]): string[] =>
  units.flatMap((unit) => (unit.kind === 'leaves' ? unit.rows.map((row) => row.tag) : [unit.kind]));

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
});
