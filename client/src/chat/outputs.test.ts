import { describe, expect, it } from 'vitest';

import { outputs } from './outputs.svelte';

describe('the answers a call-output read leaves behind', () => {
  it('hands a call its answer back by the id it asked with', () => {
    outputs.post('seat-a', 'tu-1', { kind: 'lines', lines: ['one'] });
    expect(outputs.of('tu-1')).toEqual({ kind: 'lines', lines: ['one'] });
  });

  it('lets a later answer take the place of an earlier one', () => {
    // The on-open read re-asks every open, so a file that went away between
    // two opens must be able to say so on the second.
    outputs.post('seat-a', 'tu-2', { kind: 'lines', lines: ['fresh'] });
    outputs.post('seat-a', 'tu-2', { kind: 'file_gone' });
    expect(outputs.of('tu-2')).toEqual({ kind: 'file_gone' });
  });

  it('drops one seat without touching another', () => {
    // A swap replaces the conversation, and the old occupant's answers are
    // about calls the new one never made.
    outputs.post('seat-a', 'tu-3', { kind: 'no_path' });
    outputs.post('seat-b', 'tu-4', { kind: 'no_path' });
    outputs.clear('seat-a');
    expect(outputs.of('tu-3')).toBeUndefined();
    expect(outputs.of('tu-4')).toEqual({ kind: 'no_path' });
    outputs.clear('seat-b');
  });
});
