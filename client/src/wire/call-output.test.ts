import { describe, expect, it } from 'vitest';

import { callOutputOf } from './call-output';

describe('the shape a call-output answer narrows to', () => {
  it('reads the tail the task sent', () => {
    expect(callOutputOf({ lines: ['one', 'two'] })).toEqual({
      kind: 'lines',
      lines: ['one', 'two'],
    });
  });

  it('reads the two reasons by name', () => {
    expect(callOutputOf('file_gone')).toEqual({ kind: 'file_gone' });
    expect(callOutputOf('no_path')).toEqual({ kind: 'no_path' });
  });

  it('never drops an answer it cannot name', () => {
    // Rule 25 at the narrowing: a shape this build does not know is drawn
    // plainly rather than read as nothing, or a future answer reaches the
    // row as a blank.
    expect(callOutputOf({ something: 'new' })).toEqual({ kind: 'unknown' });
    expect(callOutputOf(undefined)).toEqual({ kind: 'unknown' });
  });
});
