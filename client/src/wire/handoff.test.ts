import { describe, expect, it } from 'vitest';

import { handoffFrame, handoffKey, handoffOf, shortId } from './handoff';

const wire = (extra: Record<string, unknown> = {}): unknown => ({
  from: '9cfe315c-1111-2222-3333-444444444444',
  to: '4b21e0a7-5555-6666-7777-888888888888',
  reason: 'fill',
  fill_percent: 85,
  total_tokens: 850_000,
  max_tokens: 1_000_000,
  at_percent: 85,
  brief: {
    ground_truth: [{ label: 'seat', lines: ['Busytools/forge/lead', 'cwd /tmp'] }],
    judgment: [{ label: 'Next', lines: ['the client row'] }],
  },
  ...extra,
});

describe('the handoff record', () => {
  it('narrows the pair, the reading and both halves of the brief', () => {
    const record = handoffOf(wire());
    expect(record, 'a record with the pair narrows').not.toBeNull();
    expect(record?.reason).toBe('fill');
    expect(record?.fill_percent, 'the reading the trigger fired on').toBe(85);
    expect(record?.ground_truth[0]?.lines, 'and every line of the ground truth').toEqual([
      'Busytools/forge/lead',
      'cwd /tmp',
    ]);
    expect(record?.judgment[0]?.label, 'and the judgment by its heading').toBe('Next');
  });

  it('is nothing without the pair a row is addressed by', () => {
    expect(handoffOf(wire({ from: undefined })), 'no from means no row').toBeNull();
    expect(handoffOf(wire({ to: null })), 'and no to is the same').toBeNull();
  });

  /**
   * **A section with no label is dropped, deliberately.** The heading is the
   * writer's own word for the block; inventing one would attribute the lines
   * to a heading nobody chose. This is a record shape, not a CLI frame, so
   * the drop is a decision this test holds rather than a rendering gap.
   */
  it('drops a section with no label rather than inventing one', () => {
    const record = handoffOf(
      wire({
        brief: {
          ground_truth: [
            { lines: ['unlabelled'] },
            { label: 'seat', lines: ['Busytools/forge/lead'] },
            {},
          ],
          judgment: [{ label: 7, lines: ['not a heading'] }],
        },
      }),
    );
    expect(
      record?.ground_truth.map((section) => section.label),
      'only the section that names itself is drawn',
    ).toEqual(['seat']);
    expect(record?.judgment, 'a non-string label is no label either').toEqual([]);
  });

  /**
   * **The chat mints the row's frame from a record it has already narrowed**,
   * and the fold reads every frame back through this narrowing - so a frame
   * written in the narrowed shape loses both halves.
   */
  it('round-trips a record through the frame the row is drawn from', () => {
    const record = handoffOf(wire());
    expect(record, 'a record to put on a frame').not.toBeNull();
    if (record === null) return;
    expect(handoffOf(handoffFrame(record).handoff), 'the frame narrows back to it').toEqual(record);
  });

  it('keys the row by the pair, so a reconnect lands on one row', () => {
    const record = handoffOf(wire());
    expect(record).not.toBeNull();
    if (record === null) return;
    expect(handoffKey(record)).toBe(
      'handoff:9cfe315c-1111-2222-3333-444444444444:4b21e0a7-5555-6666-7777-888888888888',
    );
    expect(shortId(record.from), 'and the row names each session short').toBe('9cfe315c');
  });
});
