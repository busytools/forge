import { describe, expect, it } from 'vitest';

import { handoffKey, handoffOf, shortId } from './handoff';

const wire = (extra: Record<string, unknown> = {}): unknown => ({
  from: '9cfe315c-1111-2222-3333-444444444444',
  to: '4b21e0a7-5555-6666-7777-888888888888',
  reason: 'fill',
  fill_percent: 85,
  total_tokens: 850_000,
  max_tokens: 1_000_000,
  compaction_count: 1,
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

  it('drops a section with no label rather than inventing one', () => {
    const record = handoffOf(
      wire({ brief: { ground_truth: [{ lines: ['unlabelled'] }, { label: 'seat', lines: [] }] } }),
    );
    expect(
      record?.ground_truth.map((section) => section.label),
      'only labelled sections',
    ).toEqual(['seat']);
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
