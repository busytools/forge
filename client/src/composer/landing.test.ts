/**
 * The rules a landing memory keeps, which every surface that dictates shares:
 * whose words a notice is, when it has already been placed, and how the same
 * notice handed over again differs from a take that spoke the same words.
 */

import { beforeEach, describe, expect, it } from 'vitest';

import { Landing } from './landing';
import type { Notice } from './wire';

// The storage the production code reads to tell its own re-hand from another
// client's. A fresh map per test keeps one test's take out of the next.
const store = new Map<string, string>();
globalThis.localStorage = {
  getItem: (key: string) => store.get(key) ?? null,
  setItem: (key: string, value: string) => void store.set(key, value),
  removeItem: (key: string) => void store.delete(key),
  clear: () => store.clear(),
  key: (index: number) => [...store.keys()][index] ?? null,
  get length() {
    return store.size;
  },
};

beforeEach(() => {
  store.clear();
});

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
  /**
   * **The arm is what a surface about to close must wait on.** A page that
   * navigates while a take's words are still in flight loses them: the
   * landing names this surface, and no other one will place it.
   */
  it('reports the arm until a notice spends it or the take is dropped', () => {
    const landing = new Landing('composer');
    expect(landing.armed, 'nothing is owed before a press').toBe(false);
    landing.arm(SEAT, null);
    expect(landing.armed, 'a press owes a landing').toBe(true);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
    expect(landing.armed, 'the notice spent the arm').toBe(false);

    landing.arm(SEAT, words);
    landing.drop();
    expect(landing.armed, 'an abandoned take owes nothing').toBe(false);
  });

  it('places its words once, and remembers them for the cut row', () => {
    const landing = new Landing('composer');
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
    const landing = new Landing('composer');
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words)).toBe('this read is the whole input');
  });

  /**
   * **The same words a second time are a landing of their own.** The record
   * hands a notice over on every write, so what tells a re-read from a fresh
   * take is the take's own number - not its words, which can repeat.
   */
  it('places the same words again when another take spoke them', () => {
    const landing = new Landing('composer');
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
    const landing = new Landing('composer');
    landing.arm(SEAT, words);
    expect(landing.place(SEAT, words), 'the notice it was set under places nothing').toBeNull();
    expect(landing.place(SEAT, words), 'however many times it is handed over').toBeNull();
    expect(landing.place(SEAT, other), "and the take's own landing still places").toBe(
      'looks good',
    );
  });

  it("is not this page's when no take of its own is behind it", () => {
    const landing = new Landing('composer');
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
    // A reloaded page still holds the generation it dictated: the seed stands
    // in for the write a prior page's live take left behind.
    globalThis.localStorage?.setItem(`forge.dictate.take.${SEAT}`, '1');
    const carried: Notice = {
      kind: 'landed',
      text: 'this read is the whole input',
      truncated: false,
      generation: 1,
      carried: true,
    };
    const landing = new Landing('composer');
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
   * **A carried notice from another client places nothing.** The record
   * re-hands the seat's landed words to every reader, so without this check a
   * web dictation lands in the desktop's composer as well. The generation this
   * client last dictated on the seat is what tells its own re-hand from
   * another client's.
   */
  it('places nothing for a carried notice another client dictated', () => {
    // This client's last take was generation 1; the re-handed notice is 2.
    globalThis.localStorage?.setItem(`forge.dictate.take.${SEAT}`, '1');
    const foreign: Notice = {
      kind: 'landed',
      text: 'spoken on the web',
      truncated: false,
      generation: 2,
      carried: true,
    };
    const landing = new Landing('composer');
    expect(
      landing.place(SEAT, foreign),
      "another client's carried words are not this page's to place",
    ).toBeNull();
    expect(landing.holds(foreign), 'and the box holds none of them').toBe(false);
  });

  /**
   * **The same rejection when this client has never dictated on the seat.**
   * A second client attaches to a seat with no storage of its own, and the
   * record hands it the first client's carried words: it must place nothing.
   */
  it('places nothing for a carried notice on a seat this client never dictated', () => {
    const foreign: Notice = {
      kind: 'landed',
      text: 'spoken on the desktop',
      truncated: false,
      generation: 4,
      carried: true,
    };
    const landing = new Landing('composer');
    expect(
      landing.place(SEAT, foreign),
      'a seat this client never dictated on has no generation to match',
    ).toBeNull();
  });

  /**
   * **A carried notice places for a server older than the generation field.**
   * An older server cannot route per client, so its re-hand places as it
   * always did - the guard is skipped when no generation arrives.
   */
  it('places carried words a server older than the generation field re-hands', () => {
    const olderCarried: Notice = {
      kind: 'landed',
      text: 'old server words',
      truncated: false,
      carried: true,
    };
    const landing = new Landing('composer');
    expect(
      landing.place(SEAT, olderCarried),
      'the old server cannot route per client, so its re-hand places',
    ).toBe('old server words');
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
    const landing = new Landing('composer');
    landing.arm(SEAT, carried);
    expect(landing.place(SEAT, null), 'the take start clears the record').toBeNull();
    expect(landing.place(SEAT, carried), 'a read re-handing them places nothing').toBeNull();
    expect(landing.place(SEAT, spoken), "and the take's own landing still places").toBe(
      'new spoken',
    );
  });

  /**
   * **The armed take's own landing is placed from the record's re-hand.**
   * A whole-record read can be taken after the landing frame and hand the
   * words back as `carried`, with the frame itself never arriving - and the
   * take's generation is what tells those words from an older re-hand
   * (#2283): the arm is spent and the words placed, rather than the arm
   * standing for ever and the words arriving late or not at all.
   */
  it("places the armed take's own carried landing and spends the arm", () => {
    const own: Notice = {
      kind: 'landed',
      text: 'new spoken',
      truncated: false,
      generation: 2,
      carried: true,
    };
    const landing = new Landing('composer');
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, own, 2), "the record's copy of this take's landing places").toBe(
      'new spoken',
    );
    expect(landing.armed, 'and the arm is spent by it').toBe(false);
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
    const landing = new Landing('composer');
    landing.arm(SEAT, line);
    expect(
      landing.place(SEAT, carried, 3),
      "the older unsent words are not this take's, even with the take named",
    ).toBeNull();
    expect(
      landing.place(SEAT, spoken),
      "and the take's own landing still places: the arm was not spent",
    ).toBe('new spoken');
  });

  /**
   * **A landing names the surface that began it, and another surface places
   * nothing**. The record keeps the seat's words until a prompt sends
   * them, so it re-hands a review box's words to whatever page reads next;
   * without this the composer took them on its next mount.
   */
  it('places nothing for a landing another surface began', () => {
    const landing = new Landing('composer');
    const foreign: Notice = { ...words, surface: 'review' };
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, foreign), 'the review box began these words').toBeNull();
    // The arm went with the notice: the take it was set for never landed (the
    // server registered the other surface's stream), so it cannot ride the
    // next one.
    expect(landing.place(SEAT, other), "and the spent arm is not the next notice's").toBeNull();
  });

  it("places its own surface's landing, live and carried", () => {
    const live = new Landing('review');
    live.arm(SEAT, null);
    expect(live.place(SEAT, { ...words, surface: 'review' }, 1), 'live').toBe(
      'this read is the whole input',
    );
    expect(
      globalThis.localStorage?.getItem(`forge.dictate.take.${SEAT}`),
      'the live take leaves the generation a reload reads',
    ).toBe('1');
    const carried = new Landing('review');
    expect(
      carried.place(SEAT, { ...words, surface: 'review', carried: true }),
      'and a re-hand of its own words after a reload',
    ).toBe('this read is the whole input');
  });

  /**
   * **A fold take's landing seeds the generation a reload reads** (#2896). The
   * record is written once per painted frame, so a quick take arrives as a
   * landing with no take in flight. It places via the arm, and the generation
   * it carries must be stored: without it, a reload finds nothing to match and
   * drops the words the server re-hands.
   */
  it("seeds the generation when a fold take's landing arrives with no take in flight", () => {
    const landing = new Landing('composer');
    landing.arm(SEAT, null);
    expect(landing.place(SEAT, words), "the fold take's words place").toBe(
      'this read is the whole input',
    );
    expect(
      globalThis.localStorage?.getItem(`forge.dictate.take.${SEAT}`),
      'the landing seeds the generation a reload reads',
    ).toBe('1');
    landing.placed(words);
    const reloaded = new Landing('composer');
    expect(
      reloaded.place(SEAT, { ...words, carried: true }),
      'so a reload after it places the re-handed words',
    ).toBe('this read is the whole input');
  });

  it('places only for the seat its arm was set on, and keeps that arm', () => {
    // The arm is the seat's, as a page's boxes are: a take that outlives a
    // visit elsewhere still lands on the seat it began on.
    const landing = new Landing('composer');
    landing.arm(SEAT, null);
    expect(landing.place(ELSEWHERE, other), 'another seat has no arm of its own').toBeNull();
    expect(landing.place(SEAT, words), 'and the arm is still the one it was set on').toBe(
      'this read is the whole input',
    );
  });

  it('places words only: a line about a take is not words for the box', () => {
    const landing = new Landing('composer');
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
    const landing = new Landing('composer');
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
    const landing = new Landing('composer');
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
