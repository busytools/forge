/**
 * What a landed take means to the review page: whose words they are, when
 * they have already been placed, and when a fresh landing is the same words
 * again.
 */

import { describe, expect, it } from 'vitest';

import type { Notice } from '../composer/wire';
import { Landing } from './dictate';

const words: Notice = { kind: 'landed', text: 'this read is the whole input', truncated: false };
const other: Notice = { kind: 'landed', text: 'looks good', truncated: false };
const line: Notice = { kind: 'line', tone: 'bad', text: 'no input device is available' };

const SEAT = 'session:Busytools/forge/lead';
const ELSEWHERE = 'session:Busytools/forge/worker';

describe('a landing', () => {
  it('places its words once', () => {
    const landing = new Landing();
    // The record holds the take first, and its notice lands after it.
    landing.place(SEAT, true, null);
    expect(landing.place(SEAT, false, words)).toBe('this read is the whole input');
    expect(landing.place(SEAT, false, words), 'the same notice is not placed twice').toBeNull();
  });

  /**
   * **The second take that says the same thing.** A take starting clears the
   * record's notice, and a memory that outlived that would silently drop the
   * words the reader spoke again - with nothing on screen saying why.
   */
  it('places the same words again after a take started, which cleared them', () => {
    const landing = new Landing();
    landing.place(SEAT, true, null);
    expect(landing.place(SEAT, false, other)).toBe('looks good');

    expect(landing.place(SEAT, true, null), 'a take starting places nothing').toBeNull();
    expect(landing.place(SEAT, false, other), 'and the same words are a landing of their own').toBe(
      'looks good',
    );
  });

  it("is not this page's when it never watched the take", () => {
    // A page that attaches finds the previous take's notice still held; its
    // words are the box's only if this page saw the take that left them.
    const landing = new Landing();
    expect(landing.place(SEAT, false, words)).toBeNull();
    expect(landing.watched, 'and nothing was watched').toBe(false);
  });

  it('forgets the seat it was watching when the page moves to another', () => {
    // The record in hand can be another seat's between two reads, and a
    // notice it holds is one this page never watched.
    const landing = new Landing();
    landing.place(SEAT, true, null);
    expect(landing.place(SEAT, false, words)).toBe('this read is the whole input');
    expect(landing.place(ELSEWHERE, false, other), 'another seat is unwatched').toBeNull();

    landing.place(SEAT, true, null);
    expect(
      landing.place(SEAT, false, words),
      'and on the seat again it is one this page has watched once more',
    ).toBe('this read is the whole input');
  });

  it('places words only: a line about a take is not words for the box', () => {
    const landing = new Landing();
    expect(landing.place(SEAT, true, line)).toBeNull();
    expect(landing.place(SEAT, false, words)).toBe('this read is the whole input');
  });
});
