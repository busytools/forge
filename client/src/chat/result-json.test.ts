import { describe, expect, it } from 'vitest';

import { parsedTextPastClock } from './result-json';

/** A tool result's content blocks as the wire delivers them. */
function content(text: string) {
  return [{ type: 'text', text }];
}

describe('parsedTextPastClock', () => {
  it('reads the array past a leading clock line', () => {
    const rows = [{ id: 't-1', subject: 'Sweep the worktrees' }];
    const text = `now 2026-01-02T03:04:05Z\n${JSON.stringify(rows, null, 2)}`;
    expect(parsedTextPastClock(content(text))).toEqual(rows);
  });

  it('answers null for a body that is not JSON, never the raw body', () => {
    // The card layer cannot tell this null from the helper returning the raw
    // text - forgeCardOf falls to the raw row for either - so the contract is
    // pinned here, where the difference is visible.
    expect(parsedTextPastClock(content('now 2026-01-02T03:04:05Z\nnot json at all'))).toBeNull();
  });
});
