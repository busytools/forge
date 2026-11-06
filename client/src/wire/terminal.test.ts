import { describe, expect, it } from 'vitest';

import {
  TERMINAL_INPUT_TAG,
  TERMINAL_OUTPUT_TAG,
  terminalInputFrame,
  terminalOutputFrame,
  terminalStateFrom,
  terminalSummaryFrom,
} from './terminal';

describe('the terminal wire', () => {
  it('decodes one output frame as the id, the sequence and the bytes', () => {
    const bytes = new Uint8Array([TERMINAL_OUTPUT_TAG, 7, 0, 9, 0, 0, 0, 0x68, 0x69]);
    expect(terminalOutputFrame(bytes)).toEqual({
      id: 7,
      seq: 9,
      bytes: new Uint8Array([0x68, 0x69]),
    });
  });

  it('refuses a frame shorter than its header rather than drawing it', () => {
    expect(terminalOutputFrame(new Uint8Array([TERMINAL_OUTPUT_TAG, 1, 2]))).toBeNull();
  });

  it('builds one input frame as the tag, the id and the keystrokes', () => {
    expect(terminalInputFrame(3, new Uint8Array([0x6c, 0x73]))).toEqual(
      new Uint8Array([TERMINAL_INPUT_TAG, 3, 0, 0x6c, 0x73]),
    );
  });

  it('narrows a summary and keeps an unknown state on the safe side', () => {
    const summary = terminalSummaryFrom({
      id: 1,
      shell: '/bin/zsh',
      cwd: '/tmp',
      cols: 120,
      rows: 30,
      alive: true,
      state: { running: true, source: 'exact', input_expected: null, exit: null },
      title: 'zsh',
      held: false,
    });
    expect(summary?.shell).toBe('/bin/zsh');
    expect(summary?.state.running).toBe(true);
    expect(summary?.state.source).toBe('exact');
    expect(terminalSummaryFrom(null)).toBeNull();
  });

  it('treats a source it does not know as the heuristic rather than exact', () => {
    expect(terminalStateFrom({ running: false, source: 'something-new' }).source).toBe('heuristic');
  });
});
