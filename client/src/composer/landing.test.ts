/**
 * The rules a landing memory keeps, which every surface that dictates shares:
 * whose words a notice is, when it has already been placed, and how the same
 * notice handed over again differs from a take that spoke the same words.
 */

import { describe, expect, it } from 'vitest';

import { Landing } from './landing';
import type { Notice } from './wire';

const words: Notice = {
  kind: 'landed',
  text: 'this read is the whole input',
  truncated: false,
  generation: 1,
};
const other: Notice = { kind: 'landed', text: 'looks good', truncated: false, generation: 2 };
/** The same words, spoken into a take of their own. */
const again: Notice = {
  kind: 'landed',
  text: 'this read is the whole input',
  truncated: false,
  generation: 3,
};
const line: Notice = { kind: 'line', tone: 'bad', text: 'no input device is available' };
/** A record from a server older than the generation field. */
const older: Notice = { kind: 'landed', text: 'this read is the whole input', truncated: false };

const SEAT = 'session:Busytools/forge/lead';
const ELSEWHERE = 'session:Busytools/forge/worker';

describe('a landing', () => {
  it('places its words once, and remembers them for the cut row', () => {
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    landing.placed(words);
    // The record hands the same notice over on every write.
    expect(landing.place(SEAT, words), 'the same notice is not placed twice').toBeNull();
    expect(landing.holds(words), 'and the cut row still finds the words it took').toBe(true);
  });

  it('places words for a take the record never showed a take for', () => {
    // The record is written once per painted frame, so a take's start and its
    // end can fold into one write: the arm is the press's own.
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
  });

  /**
   * **The same words a second time are a landing of their own.** The record
   * hands a notice over on every write, so what tells a re-read from a fresh
   * take is the take's own number - not its words, which can repeat.
   */
  it('places the same words again when another take spoke them', () => {
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    landing.placed(words);

    // The second take was quick: no write between its press and its landing
    // ever cleared the first notice, so the press was made under it.
    landing.arm(SEAT, words);
    expect(landing.place(SEAT, again), 'the second take lands its own words').toBe(
      'this read is the whole input',
    );
  });

  /**
   * **The previous take's notice, still on the record at the press.** Any
   * write between the press and the take's start runs the landing with that
   * notice: it is skipped, not consumed, so the arm survives to the landing it
   * belongs to.
   */
  it('skips the notice the arm was set under without spending the arm', () => {
    const landing = new Landing();
    landing.arm(SEAT, words);
    expect(landing.place(SEAT, words), 'the notice it was set under places nothing').toBeNull();
    expect(landing.place(SEAT, words), 'however many times it is handed over').toBeNull();
    expect(landing.place(SEAT, other), "and the take's own landing still places").toBe(
      'looks good',
    );
  });

  it("is not this page's when no take of its own is behind it", () => {
    const landing = new Landing();
    expect(landing.place(SEAT, words)).toBeNull();
    expect(landing.holds(words), 'and it holds none of the words').toBe(false);
  });

  /**
   * **The seat's carried words are any page's to place** (#1360). A landing
   * nobody has sent is re-handed by the record with `carried`, so a page that
   * reloaded between the landing and the send places the same words it would
   * have placed live - once, by identity, like any other landing.
   */
  it("places the seat's carried words without an arm of its own", () => {
    const carried: Notice = {
      kind: 'landed',
      text: 'this read is the whole input',
      truncated: false,
      generation: 1,
      carried: true,
    };
    const landing = new Landing();
    expect(
      landing.place(SEAT, carried),
      "a reloaded page never armed, and the seat's unsent words are still its to place",
    ).toBe('this read is the whole input');
    landing.placed(carried);
    expect(
      landing.place(SEAT, carried),
      'and the re-hand places once, like any landing',
    ).toBeNull();
    expect(landing.holds(carried), 'the cut row still finds the words it took').toBe(true);
  });

  /**
   * **A re-handed notice the arm was set under is still skipped** (#1360). A
   * whole-record read can bring the seat's unsent words back after the take's
   * start cleared them; the arm belongs to the take, so the old words must
   * not be placed over the landing that is coming.
   */
  it('skips the under notice when a read re-hands it mid-take', () => {
    const carried: Notice = {
      kind: 'landed',
      text: 'old unsent',
      truncated: false,
      generation: 1,
      carried: true,
    };
    const spoken: Notice = {
      kind: 'landed',
      text: 'new spoken',
      truncated: false,
      generation: 2,
    };
    const landing = new Landing();
    landing.arm(SEAT, carried);
    expect(landing.place(SEAT, null), 'the take start clears the record').toBeNull();
    expect(landing.place(SEAT, carried), 'a read re-handing them places nothing').toBeNull();
    expect(landing.place(SEAT, spoken), "and the take's own landing still places").toBe(
      'new spoken',
    );
  });

  /**
   * **A carried re-hand is skipped whatever the arm was set under** (#1360).
   * An intervening failed take moves `under` off the seat's unsent words, so
   * the identity rule alone would let a mid-take read place them again and
   * spend the arm - swallowing the take's own landing. The flag is the
   * discriminator, not the identity.
   */
  it('skips a carried re-hand whatever the arm was set under', () => {
    const carried: Notice = {
      kind: 'landed',
      text: 'old unsent',
      truncated: false,
      generation: 1,
      carried: true,
    };
    const line: Notice = {
      kind: 'line',
      tone: 'q',
      text: 'nothing above the floor',
      generation: 2,
    };
    const spoken: Notice = {
      kind: 'landed',
      text: 'new spoken',
      truncated: false,
      generation: 3,
    };
    const landing = new Landing();
    landing.arm(SEAT, line);
    expect(landing.place(SEAT, carried), "the older unsent words are not this take's").toBeNull();
    expect(
      landing.place(SEAT, spoken),
      "and the take's own landing still places: the arm was not spent",
    ).toBe('new spoken');
  });

  it('places only for the seat its arm was set on, and keeps that arm', () => {
    // The arm is the seat's, as a page's boxes are: a take that outlives a
    // visit elsewhere still lands on the seat it began on.
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(ELSEWHERE, other), 'another seat has no arm of its own').toBeNull();
    expect(landing.place(SEAT, words), 'and the arm is still the one it was set on').toBe(
      'this read is the whole input',
    );
  });

  it('places words only: a line about a take is not words for the box', () => {
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, line)).toBeNull();
    expect(landing.holds(line), 'and the line is not words this box holds').toBe(false);
    // The line was the take's outcome, so the arm went with it: the next
    // words need a take of their own.
    landing.arm(SEAT, line);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
  });

  /**
   * **A record from a server older than the take's number.** Kind and text are
   * all the memory has then, and they cannot tell a re-read from the same
   * words spoken again: the fallback's cost, pinned so the number's job stays
   * visible.
   */
  it('falls back to kind and text where the record carries no take number', () => {
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, older)).toBe('this read is the whole input');
    landing.placed(older);
    expect(landing.holds(older), "the words are the box's").toBe(true);
    // A fresh arm (a second take), so the spend cannot be what refuses the
    // re-handed notice: the fallback cannot tell it from words spoken again,
    // which is the cost the take's number exists to remove.
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, { ...older }), 'the re-handed notice places nothing').toBeNull();
  });

  it('a take starting keeps the placed memory: the words are still in the box', () => {
    const landing = new Landing();
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    landing.placed(words);
    // The record's notice goes null when the next take starts, before any
    // landing - and a whole-record read can re-hand it afterwards. The words
    // have not moved, so the re-hand places nothing (#1360's duplicate).
    expect(landing.place(SEAT, null), 'a take starting places nothing').toBeNull();
    expect(landing.holds(words), 'and the memory still holds the words in the box').toBe(true);
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words), 'the same notice re-handed places nothing').toBeNull();
    // **The same words into a new take are a new number**, which is what
    // lets the reader dictate them twice.
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, again), 'so the second take lands its own words').toBe(
      'this read is the whole input',
    );
  });
});
