import { describe, expect, it } from 'vitest';

import { previewNext, type PreviewState } from './preview';

const WORKING: PreviewState = { voice: 'af_heart', phase: 'working' };
const PLAYING: PreviewState = { voice: 'af_heart', phase: 'playing' };

describe('the preview state machine', () => {
  /** The press shows as working while the clip renders, then as playing for
   * the clip's whole duration - so the row names the voice being heard. */
  it('turns a working press into playing when its clip sounds', () => {
    expect(previewNext(WORKING, true, null, null)).toEqual({
      voice: 'af_heart',
      phase: 'playing',
    });
    expect(previewNext(PLAYING, true, null, null)).toBe(PLAYING);
  });

  it('clears the row when the clip ends', () => {
    expect(previewNext(PLAYING, false, null, null)).toBeNull();
  });

  /** A second press is one assignment in the page, and this is the row it
   * lands on: the old row is no longer playing because the state moved. */
  it('moves the press to the voice pressed next', () => {
    const second: PreviewState = { voice: 'bf_emma', phase: 'working' };
    expect(previewNext(second, false, null, null)).toBe(second);
    expect(previewNext(second, true, null, null)).toEqual({
      voice: 'bf_emma',
      phase: 'playing',
    });
  });

  it('names a failure on the pressed row', () => {
    expect(previewNext(WORKING, false, 'the render died', null)).toEqual({
      voice: 'af_heart',
      phase: 'failed',
    });
  });

  it('does not fail a fresh press for the failure already on screen', () => {
    expect(previewNext(WORKING, false, 'the render died', 'the render died')).toBe(WORKING);
  });
});
