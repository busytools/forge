import { describe, expect, it } from 'vitest';

import { latestCompaction } from './compaction-jump';

const boundary = (id: string): unknown => ({
  type: 'system',
  subtype: 'compact_boundary',
  uuid: id,
  compact_metadata: { trigger: 'auto', pre_tokens: 100, post_tokens: 10 },
});

const said = (text: string): unknown => ({
  type: 'assistant',
  uuid: `a-${text}`,
  message: {
    id: `m-${text}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text }],
  },
});

describe('finding the latest compaction', () => {
  it('answers the last turn that holds one, and null when none does', () => {
    // The header's count opens the NEWEST cut, so the search runs from the
    // end: a jump that stopped at the first would take the reader to the
    // oldest one instead.
    const turns = [
      { messages: [boundary('cb-1')] },
      { messages: [said('between')] },
      { messages: [boundary('cb-2'), said('after')] },
    ];

    expect(latestCompaction(turns), 'the newest cut, not a chosen one').toBe(2);
    expect(latestCompaction([{ messages: [said('plain')] }]), 'no boundary, no jump').toBeNull();
    expect(latestCompaction([]), 'an empty history answers nothing').toBeNull();
  });
});
