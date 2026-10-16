import { describe, expect, it } from 'vitest';

import type { ReviewSetRecord, ReviewThreadRecord } from '../session/wire';
import {
  ageOf,
  authorName,
  awaitsReviewer,
  canvasLine,
  latestRound,
  loopState,
  outdatedThreads,
  parkedReason,
  roundHasWork,
  roundTally,
  roundsNewestFirst,
  statusName,
  threadsInRound,
  threadsInScope,
  turnRound,
} from './view';

function thread(
  id: string,
  status: ReviewThreadRecord['status'],
  turns: { author: 'you' | 'agent' | 'unknown'; review_id: string | null }[],
  extra: Partial<ReviewThreadRecord> = {},
): ReviewThreadRecord {
  return {
    id,
    anchor: {
      path: 'a.rs',
      side: 'new',
      line: 1,
      content_hash: '1',
      context: [],
      base_ref: 'main',
    },
    comments: turns.map((turn, i) => ({
      author:
        turn.author === 'you'
          ? { kind: 'user' as const }
          : turn.author === 'agent'
            ? { kind: 'agent' as const, label: 'impl' }
            : { kind: 'unknown' as const, raw: 'a shape this build cannot name' },
      text: `turn ${i}`,
      at: '2026-10-10T00:00:00Z',
      review_id: turn.review_id,
    })),
    status,
    created_at: '2026-10-10T00:00:00Z',
    updated_at: '2026-10-10T00:00:00Z',
    commit: null,
    resolution: null,
    ...extra,
  };
}

function round(id: string, number: number): ReviewSetRecord {
  return { id, number, summary: null, created_at: '2026-10-10T00:00:00Z' };
}

describe('whether a thread awaits the reviewer', () => {
  it('is the agent turn last on an answered thread, and nothing else', () => {
    // The plausible regression is counting `open` as waiting: it is the state
    // a reopen and the code-coming-back path both land in, and both mean the
    // reviewer handed the work back rather than waiting on it.
    expect(
      awaitsReviewer(
        thread('a', 'addressed', [
          { author: 'you', review_id: null },
          { author: 'agent', review_id: null },
        ]),
      ),
      'answered, unread',
    ).toBe(true);
    expect(
      awaitsReviewer(thread('b', 'open', [{ author: 'agent', review_id: null }])),
      'open never waits',
    ).toBe(false);
    expect(
      awaitsReviewer(
        thread('c', 'addressed', [
          { author: 'agent', review_id: null },
          { author: 'you', review_id: null },
        ]),
      ),
      'the reviewer replied',
    ).toBe(false);
    expect(
      awaitsReviewer(thread('d', 'outdated', [{ author: 'agent', review_id: null }])),
      'a drifted anchor still carries the answer',
    ).toBe(true);
    expect(
      awaitsReviewer(thread('e', 'resolved', [{ author: 'agent', review_id: null }])),
      'resolved is read and done',
    ).toBe(false);
    // An author this build cannot name is drawn, never counted: the server
    // counts `ReviewAuthor::Agent` alone, so counting anything else here
    // would make this page's tally disagree with every other surface's.
    expect(
      awaitsReviewer(thread('f', 'addressed', [{ author: 'unknown', review_id: null }])),
      'an unnamed author is not an agent turn',
    ).toBe(false);
  });
});

describe('a round and its members', () => {
  it('counts a thread under every round its turns were sealed into', () => {
    // A reply filed into a later round makes the thread a member of both, the
    // way the server tallies it; counting only the newest would make an
    // earlier round read as empty.
    const spanning = thread('s', 'addressed', [
      { author: 'you', review_id: 'r1' },
      { author: 'agent', review_id: null },
      { author: 'you', review_id: 'r2' },
    ]);
    // One member of each state under r1, so swapping any two counter arms -
    // or dropping one - moves the tally rather than surviving it.
    const other = thread('o', 'open', [{ author: 'you', review_id: 'r1' }]);
    const done = thread('d', 'resolved', [{ author: 'you', review_id: 'r1' }]);
    const drifted = thread('p', 'outdated', [{ author: 'you', review_id: 'r1' }]);
    const threads = [spanning, other, done, drifted];
    const r1 = round('r1', 1);
    const r2 = round('r2', 2);

    expect(
      threadsInRound(threads, 'r1').map((t) => t.id),
      'every member under r1',
    ).toEqual(['s', 'o', 'd', 'p']);
    expect(
      threadsInRound(threads, 'r2').map((t) => t.id),
      'the spanning one under r2',
    ).toEqual(['s']);
    expect(roundTally(threads, r1), 'r1 tallies one of each state').toEqual({
      total: 4,
      open: 1,
      addressed: 1,
      resolved: 1,
      outdated: 1,
      named: 0,
    });
    expect(turnRound([r1, r2], { review_id: 'r1' }), 'a sealed turn names its round').toBe(1);
    expect(turnRound([r1, r2], { review_id: null }), 'an agent turn is in no round').toBeNull();
    expect(
      roundsNewestFirst([r1, r2]).map((r) => r.number),
      'newest first',
    ).toEqual([2, 1]);
  });

  it('names the round a reopen is about: the thread latest filed turn', () => {
    // The view this client replaced read it the same way - the most recent
    // review the thread has a turn in, because that is the exchange the
    // reviewer is unhappy with. The origin round would point the worker at
    // an exchange nobody reopened.
    const spanning = thread('s', 'resolved', [
      { author: 'you', review_id: 'r1' },
      { author: 'agent', review_id: null },
      { author: 'you', review_id: 'r2' },
    ]);
    const answered = thread('a', 'open', [
      { author: 'you', review_id: 'r1' },
      { author: 'agent', review_id: null },
    ]);
    const drafted = thread('d', 'open', [{ author: 'you', review_id: null }]);
    const r1 = round('r1', 1);
    const r2 = round('r2', 2);

    expect(latestRound([r1, r2], spanning), 'the newest round it was sealed into').toBe(2);
    expect(latestRound([r1, r2], answered), 'an unfiled answer does not clear it').toBe(1);
    expect(latestRound([r1, r2], drafted), 'nothing filed names no round').toBeNull();
    expect(
      latestRound([r1], thread('gone', 'open', [{ author: 'you', review_id: 'r9' }])),
      'a turn naming a round the read does not carry names none',
    ).toBeNull();
  });

  it('tells a round with work in it from one that is only housekeeping', () => {
    // The view this client replaced sealed by this predicate: a round whose
    // authored threads are all resolved or outdated has nothing for the
    // worker to act on, so the finish files it and nudges nobody.
    const open = thread('o', 'open', [{ author: 'you', review_id: null }]);
    const answered = thread('a', 'addressed', [{ author: 'you', review_id: null }]);
    const done = thread('d', 'resolved', [{ author: 'you', review_id: null }]);
    const drifted = thread('p', 'outdated', [{ author: 'you', review_id: null }]);
    const odd = thread('u', { unknown: 'Pondered' }, [{ author: 'you', review_id: null }]);
    const threads = [open, answered, done, drifted, odd];

    expect(roundHasWork(threads, ['o']), 'an open comment is work').toBe(true);
    expect(roundHasWork(threads, ['d', 'a']), 'and an addressed one in the mix').toBe(true);
    expect(roundHasWork(threads, ['d']), 'a resolved comment alone is not').toBe(false);
    expect(roundHasWork(threads, ['d', 'p']), 'nor resolved and outdated together').toBe(false);
    expect(roundHasWork(threads, ['u']), 'a state this build cannot name counts as work').toBe(
      true,
    );
    expect(
      roundHasWork(threads, ['gone']),
      'a thread the read does not carry cannot be shown settled',
    ).toBe(true);
    expect(roundHasWork(threads, []), 'nothing named is no work').toBe(false);
  });

  it('counts a state this build cannot name without folding it into a neighbour', () => {
    const odd = thread('odd', { unknown: 'Pondered' }, [{ author: 'you', review_id: 'r1' }]);
    const tally = roundTally([odd], round('r1', 1));
    expect(tally, 'the unnamed state is counted as itself').toEqual({
      total: 1,
      open: 0,
      addressed: 0,
      resolved: 0,
      outdated: 0,
      named: 1,
    });
    expect(statusName(odd.status), 'and reads by its own word').toBe('Pondered');
  });
});

describe('where a thread draws on the canvas', () => {
  it('places by the server landing for all changes, by the recorded line for its own commit', () => {
    // The plausible regression is trusting the recorded line for an
    // all-changes thread - exactly the guess the server's resolution exists
    // to replace - or drawing a commit's own thread at its commit-numbered
    // line in the branch's numbering.
    const placed = thread('a', 'open', [{ author: 'you', review_id: null }], {
      resolution: { state: 'placed', line: 42, moved_from: 7 },
    });
    expect(canvasLine(placed, null), 'moved draws where it landed, remembering 7').toEqual({
      line: 42,
      movedFrom: 7,
    });
    const inPlace = thread('b', 'open', [{ author: 'you', review_id: null }], {
      resolution: { state: 'placed', line: 9, moved_from: null },
    });
    expect(canvasLine(inPlace, null)).toEqual({ line: 9, movedFrom: null });
    const scoped = thread('c', 'open', [{ author: 'you', review_id: null }], {
      commit: 'abc123',
      anchor: {
        path: 'a.rs',
        side: 'new',
        line: 5,
        content_hash: '1',
        context: [],
        base_ref: 'main',
      },
    });
    expect(canvasLine(scoped, 'abc123'), 'an immutable scope keeps its own numbering').toEqual({
      line: 5,
      movedFrom: null,
    });
    expect(
      canvasLine(scoped, null),
      'and the branch draws it only where the resolver placed it, never at the commit line',
    ).toBeNull();
    const scopedPlaced = thread('d', 'open', [{ author: 'you', review_id: null }], {
      commit: 'abc123',
      resolution: { state: 'placed', line: 9, moved_from: null },
    });
    expect(
      canvasLine(scopedPlaced, null),
      'a commit thread the branch still carries draws in all changes too',
    ).toEqual({ line: 9, movedFrom: null });
    expect(
      canvasLine(scopedPlaced, 'def456'),
      "and another commit's view is not its scope",
    ).toBeNull();
    const unresolvable = thread('e', 'open', [{ author: 'you', review_id: null }], {
      resolution: { state: 'outdated', reason: 'gone' },
    });
    expect(canvasLine(unresolvable, null), 'nothing to place against').toBeNull();
    expect(
      canvasLine(thread('f', 'open', [{ author: 'you', review_id: null }]), null),
      'no landing carried: not guessed onto the recorded line',
    ).toBeNull();
    const allChanges = thread('g', 'open', [{ author: 'you', review_id: null }], {
      resolution: { state: 'placed', line: 3, moved_from: null },
    });
    expect(
      canvasLine(allChanges, 'abc123'),
      "an all-changes thread has no landing in a commit's numbering",
    ).toBeNull();
  });

  it('draws every thread the scope can place, and parks only the unplaceable', () => {
    const all = thread('all', 'open', [{ author: 'you', review_id: null }], {
      resolution: { state: 'placed', line: 3, moved_from: null },
    });
    // Written inside a commit, and the branch's diff still carries it: it
    // draws in both scopes.
    const scoped = thread('scoped', 'open', [{ author: 'you', review_id: null }], {
      commit: 'abc123',
      resolution: { state: 'placed', line: 8, moved_from: null },
    });
    // Written inside a commit the branch no longer shows where it was.
    const scopedGone = thread('scoped-gone', 'open', [{ author: 'you', review_id: null }], {
      commit: 'abc123',
      resolution: { state: 'outdated', reason: 'gone' },
    });
    const parked = thread('parked', 'open', [{ author: 'you', review_id: null }], {
      resolution: { state: 'outdated', reason: { ambiguous: { matches: 2 } } },
    });
    const resolved = thread('resolved', 'resolved', [{ author: 'you', review_id: null }], {
      resolution: { state: 'outdated', reason: 'gone' },
    });
    const threads = [all, scoped, scopedGone, parked, resolved];

    expect(
      threadsInScope(threads, null).map((t) => t.id),
      'all changes draws every placed thread',
    ).toEqual(['all', 'scoped']);
    expect(
      threadsInScope(threads, 'abc123').map((t) => t.id),
      'a commit draws its own',
    ).toEqual(['scoped', 'scoped-gone']);
    expect(
      outdatedThreads(threads).map((t) => t.id),
      'the unplaceable park; a resolved one never does',
    ).toEqual(['scoped-gone', 'parked']);
    expect(parkedReason(parked), 'an ambiguous landing says how many').toBe(
      'its code matches 2 places',
    );
    expect(parkedReason(scopedGone), 'a landing that says gone says so').toBe(
      'the code it was written against is gone',
    );
    expect(
      parkedReason(thread('draft', 'open', [{ author: 'you', review_id: null }])),
      'and a thread whose read carried no landing draws no reason',
    ).toBe('');
    expect(
      outdatedThreads([
        thread('store', 'outdated', [{ author: 'you', review_id: null }], {
          resolution: { state: 'placed', line: 4, moved_from: null },
        }),
      ]),
      'code that came back un-parks: the fresh landing wins over the store',
    ).toEqual([]);
  });
});

describe('the loop state and the names it draws', () => {
  it('counts what waits and what is open', () => {
    const threads = [
      thread('a', 'addressed', [{ author: 'agent', review_id: null }]),
      thread('b', 'open', [{ author: 'you', review_id: null }]),
      thread('c', 'resolved', [{ author: 'agent', review_id: null }]),
    ];
    expect(loopState(threads)).toEqual({ waiting: 1, open: 1 });
  });

  it('names an unnamed author and an unparsable age without inventing either', () => {
    expect(authorName({ kind: 'unknown', raw: 7 })).toBe('unknown');
    expect(authorName({ kind: 'agent', label: 'impl' })).toBe('impl');
    expect(authorName({ kind: 'user' })).toBe('you');
    expect(ageOf('not a date', Date.now()), 'unparsable draws nothing').toBe('');
  });
});
