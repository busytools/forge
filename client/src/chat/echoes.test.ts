import { describe, expect, it } from 'vitest';

import { ownWords } from './echoes.svelte';

/** A frame as the wire shapes one, which is all this reads. */
const frame = (type: string, content: unknown[]): unknown => ({
  type,
  uuid: 'u-1',
  message: { role: type, content },
});

describe('the words a frame carries', () => {
  it('reads the reader own prompt in both carriers, and nothing else', () => {
    // A prompt that starts a turn arrives as the message's own text.
    expect(ownWords(frame('user', [{ type: 'text', text: 'run the gate' }]))).toEqual([
      'run the gate',
    ]);

    // One sent while a turn is already running is held by the CLI as a
    // `queued_command` block instead - the shape a mid-turn prompt reaches the
    // page as, since the CLI never echoes one on stream-json. A reconcile that
    // knew only the first carrier would leave the row saying "sending" for the
    // rest of the turn.
    expect(
      ownWords(
        frame('user', [
          { type: 'queued_command', commandMode: 'prompt', prompt: 'run the gate' },
        ]),
      ),
    ).toEqual(['run the gate']);

    // The model's own words are not the reader's, and neither is the harness's
    // completion notice - which is a queued block too, and nobody typed it.
    expect(ownWords(frame('assistant', [{ type: 'text', text: 'run the gate' }]))).toEqual([]);
    expect(
      ownWords(
        frame('user', [
          { type: 'queued_command', commandMode: 'task-notification', prompt: 'run the gate' },
        ]),
      ),
    ).toEqual([]);
  });
});
