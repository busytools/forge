import { describe, expect, it } from 'vitest';

import type { ReviewSetRecord, ReviewThreadRecord } from '../session/wire';
import {
  ageOf,
  authorName,
  awaitsReviewer,
  loopState,
  roundTally,
  roundsNewestFirst,
  statusName,
  threadsInRound,
  turnRound,
} from './view';

function thread(
  id: string,
  status: ReviewThreadRecord['status'],
  turns: { author: 'you' | 'agent'; review_id: string | null }[],
): ReviewThreadRecord {
  return {
    id,
    anchor: { path: 'a.rs', side: 'new', line: 1, content_hash: 1, context: [], base_ref: 'main' },
    comments: turns.map((turn, i) => ({
      author:
        turn.author === 'you'
          ? { kind: 'user' as const }
          : { kind: 'agent' as const, label: 'impl' },
      text: `turn ${i}`,
      at: '2026-10-10T00:00:00Z',
      review_id: turn.review_id,
    })),
    status,
    created_at: '2026-10-10T00:00:00Z',
    updated_at: '2026-10-10T00:00:00Z',
    commit: null,
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
    const other = thread('o', 'open', [{ author: 'you', review_id: 'r1' }]);
    const threads = [spanning, other];
    const r1 = round('r1', 1);
    const r2 = round('r2', 2);

    expect(
      threadsInRound(threads, 'r1').map((t) => t.id),
      'both under r1',
    ).toEqual(['s', 'o']);
    expect(
      threadsInRound(threads, 'r2').map((t) => t.id),
      'the spanning one under r2',
    ).toEqual(['s']);
    expect(roundTally(threads, r1), 'r1 tallies both').toEqual({
      total: 2,
      open: 1,
      addressed: 1,
      resolved: 0,
      outdated: 0,
      named: 0,
    });
    expect(turnRound([r1, r2], { review_id: 'r1' }), 'a sealed turn names its round').toBe(1);
    expect(turnRound([r1, r2], { review_id: null }), 'an agent turn is in no round').toBeNull();
    expect(
      roundsNewestFirst([r1, r2]).map((r) => r.number),
      'newest first',
    ).toEqual([2, 1]);
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
