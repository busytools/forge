import { describe, expect, it } from 'vitest';

import { turnOfDispatch } from './dispatch-jump';

/** The frame shape the walk reads. */
const call = (id: string) => ({
  type: 'assistant',
  message: { content: [{ type: 'tool_use', id, name: 'Task', input: {} }] },
});

describe('finding the turn a dispatch sits in', () => {
  it('answers the turn that carries the dispatch call', () => {
    const turns = [{ messages: [{ type: 'user' }] }, { messages: [call('tu-b')] }];

    expect(turnOfDispatch(turns, 'tu-b'), 'the second turn').toBe(1);
  });

  it('answers null for a dispatch the page does not hold', () => {
    const turns = [{ messages: [call('tu-a')] }];

    expect(turnOfDispatch(turns, 'tu-gone')).toBeNull();
  });

  it('survives messages that are not call frames at all', () => {
    const turns = [
      { messages: [{ type: 'user', message: { content: 'hello' } }] },
      { messages: [call('tu-a')] },
    ];

    expect(turnOfDispatch(turns, 'tu-a')).toBe(1);
  });
});
