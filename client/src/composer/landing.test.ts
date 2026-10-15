/**
 * What a landed take means to a page: whose words they are, when they have
 * already been placed, and when a fresh landing is the same words again.
 */

import { describe, expect, it } from 'vitest';

import { Landing } from './landing';
import type { Notice } from './wire';

const words: Notice = { kind: 'landed', text: 'this read is the whole input', truncated: false };
const other: Notice = { kind: 'landed', text: 'looks good', truncated: false };
const line: Notice = { kind: 'line', tone: 'bad', text: 'no input device is available' };

const SEAT = 'session:Busytools/forge/lead';
const ELSEWHERE = 'session:Busytools/forge/worker';

describe('a landing', () => {
  it('places its words once', () => {
    const landing = new Landing();
    // The page's own take begins (the press), and its notice lands after it.
    landing.arm(SEAT);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    expect(landing.place(SEAT, words), 'the same notice is not placed twice').toBeNull();
  });

  /**
   * **The quick take the record never showed.** The record a page reads is
   * written once per painted frame, so a take's start and its end can fold
   * into one write: the arm is the press's own, and the words land with no
   * take ever read from the record.
   */
  it('places words for a take the record never showed a take for', () => {
    const landing = new Landing();
    landing.arm(SEAT);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
  });

  /**
   * **The second take that says the same thing.** A take starting clears the
   * record's notice, and a memory that outlived that would silently drop the
   * words the reader spoke again - with nothing on screen saying why.
   */
  it('places the same words again after a take started, which cleared them', () => {
    const landing = new Landing();
    landing.arm(SEAT);
    expect(landing.place(SEAT, other)).toBe('looks good');

    landing.arm(SEAT);
    expect(landing.place(SEAT, null), 'a take starting places nothing').toBeNull();
    expect(landing.place(SEAT, other), 'and the same words are a landing of their own').toBe(
      'looks good',
    );
  });

  it("is not this page's when no take of its own is behind it", () => {
    // A page that attaches has none of its own in flight: the notice's words
    // are not its to place.
    const landing = new Landing();
    expect(landing.place(SEAT, words)).toBeNull();
    expect(landing.holds(words), 'and it holds none of the words').toBe(false);
  });

  it('forgets the seat it was armed on when the page moves to another', () => {
    // The record in hand can be another seat's between two reads: an arm for
    // one seat places nothing for another.
    const landing = new Landing();
    landing.arm(SEAT);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    expect(landing.place(ELSEWHERE, other), 'another seat has no arm of its own').toBeNull();

    landing.arm(SEAT);
    expect(landing.place(SEAT, words), 'and on the seat again a new arm places').toBe(
      'this read is the whole input',
    );
  });

  /**
   * **The words the words row reads.** The cut row asks whether this page
   * holds the notice's words, and it has to hold them on every read after the
   * landing - the arm is spent by then, so the placed memory is what is left.
   */
  it('holds the notice the words it placed, and no other', () => {
    const landing = new Landing();
    landing.arm(SEAT);
    expect(landing.holds(words), 'the arm alone holds them while they land').toBe(true);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    expect(landing.holds(words), 'and the placed memory holds them after').toBe(true);
    expect(landing.holds(other), 'not the next notice').toBe(false);
  });

  it('places words only: a line about a take is not words for the box', () => {
    const landing = new Landing();
    landing.arm(SEAT);
    expect(landing.place(SEAT, line)).toBeNull();
    expect(landing.holds(line), 'and the line is not words this page holds').toBe(false);
    // The line was the take's outcome, so the arm went with it: the next words
    // need a take of their own.
    landing.arm(SEAT);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
  });
});
