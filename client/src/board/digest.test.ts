/**
 * The digest strip's own decisions: one line per count, in the window's
 * order, each carrying the items it opens onto.
 */

import { describe, expect, it } from 'vitest';

import type { AttentionView } from './window';
import { digestLines, sampleClaim } from './digest';

function attention(over: Partial<AttentionView> = {}): AttentionView {
  return {
    lastLook: 1_000,
    checkedAt: 2_000,
    agents: 2,
    moved: 0,
    landed: 0,
    expired: 0,
    merged: 0,
    items: { moved: [], landed: [], expired: [], merged: [] },
    sample: { rows: [], sampled: 0, total: 0 },
    ...over,
  };
}

describe('the digest strip', () => {
  it("draws one line per count, in the strip's order", () => {
    const lines = digestLines(attention({ merged: 1, moved: 2, landed: 3, expired: 4 }));
    expect(lines.map((line) => line.key)).toEqual([
      'merged',
      'moved',
      'landed',
      'expired',
      'sampled',
    ]);
    expect(lines.map((line) => line.count)).toEqual([1, 2, 3, 4, 0]);
    for (const line of lines) {
      expect(line.label).toBeTruthy();
    }
  });

  it('opens each count onto the items behind it', () => {
    const moved = [{ id: 't-1', subject: 'the window', word: 'answered: ship it', at: 1_500 }];
    const expired = [
      {
        id: 't-2',
        subject: 'the ask nobody answered',
        word: 'expired: no answer by its deadline, and its record stays',
        at: 1_600,
      },
    ];
    const lines = digestLines(
      attention({
        moved: 1,
        expired: 1,
        items: { moved, landed: [], expired, merged: [] },
      }),
    );
    expect(lines.find((line) => line.key === 'moved')?.items).toEqual(moved);
    expect(lines.find((line) => line.key === 'expired')?.items).toEqual(expired);
  });

  it('draws a count with nothing behind it rather than dropping the line', () => {
    const lines = digestLines(attention({ moved: 1 }));
    const moved = lines.find((line) => line.key === 'moved');
    expect(moved?.count, 'the count is drawn').toBe(1);
    expect(moved?.items, 'and its line has nothing to open').toEqual([]);
    expect(lines.find((line) => line.key === 'landed')?.count).toBe(0);
  });

  it('names the sample as sampled of total actions, with each action behind it', () => {
    const lines = digestLines(
      attention({
        sample: {
          rows: [
            { seat: 'lead', tool: 'Bash', detail: 'ls', at: 1_700 },
            { seat: 'worker', tool: 'Edit', detail: '', at: 1_800 },
          ],
          sampled: 2,
          total: 8,
        },
      }),
    );
    const sampled = lines.find((line) => line.key === 'sampled');
    expect(sampled?.countLabel).toBe('2 of 8 agent actions');
    expect(sampled?.items).toEqual([
      { id: null, subject: 'lead', word: 'Bash ls', at: 1_700 },
      { id: null, subject: 'worker', word: 'Edit', at: 1_800 },
    ]);
    expect(sampleClaim(attention({ sample: { rows: [], sampled: 2, total: 8 } }))).toBe(
      'sampled 2 of 8 agent actions',
    );
  });
});
