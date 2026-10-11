/**
 * Look scaffolding for the review page, and nothing the shipped app carries.
 *
 * **It files a real round through the real socket**: the page then draws the
 * store's own rows - the same path the terminal's `/diff` submit takes - so
 * the look is of the server's data rather than of a fixture. That is the whole
 * point of seeding instead of shipping a sample.
 *
 * **Deleted when the page's own compose and submit land** (the canvas slice),
 * at which point the page can file a round from its own controls.
 *
 * The anchors carry no content hash and a line of the author's choosing: the
 * hash exists for the terminal's resolver, which re-reads a working tree this
 * page cannot see, so a seeded hash would be a guess dressed as a fact.
 */

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/** A string only this module carries, so the bundle guard can watch for it. */
export const SEED_MARKER = 'seeded for the dev-stack look';

interface SeedThread {
  path: string;
  line: number;
  side: 'New' | 'Old';
  status: 'Open' | 'Addressed' | 'Outdated';
  context: string[];
  turns: { author: 'User' | 'Agent'; text: string }[];
}

/** How a seeded agent turn is attributed, which is what a reply carries. */
const AGENT = 'seeded-agent';

const SEEDS: SeedThread[] = [
  {
    path: 'client/src/review/Review.svelte',
    line: 74,
    side: 'New',
    status: 'Addressed',
    context: ['<h1>Review</h1>'],
    turns: [
      {
        author: 'User',
        text: 'The header says Review but the seat it belongs to is only in the back link. Name the seat here too?',
      },
      {
        author: 'Agent',
        text: 'Named it - the header now carries the branch beside the title.',
      },
    ],
  },
  {
    path: 'client/src/review/view.ts',
    line: 54,
    side: 'New',
    status: 'Open',
    context: ['export function loopState(threads: ReviewThreadRecord[]): LoopState {'],
    turns: [
      {
        author: 'User',
        text: 'This counts waits and opens; should it also say how many rounds are unsettled?',
      },
    ],
  },
  {
    path: 'client/src/session/wire.ts',
    line: 199,
    side: 'Old',
    status: 'Outdated',
    context: [
      'export type ReadWire<T> =',
      "  | { outcome: 'read'; value: T }",
      "  | { outcome: 'failed'; value: string }",
    ],
    turns: [
      {
        author: 'User',
        text: 'The unknown-outcome arm needs to survive the parse - a read that answered something is not a failed one.',
      },
    ],
  },
];

/**
 * File one round on the seat's branch, over its own connection.
 *
 * **Two phases, because sealing reopens an answered thread.** The store
 * flips an `Addressed` thread back to `Open` when `submit_review` seals a
 * new reviewer turn into it (`store/review.rs`), which is the reviewer
 * speaking again and the ball going back to the agent. So the round is
 * sealed first, with every thread Open, and only then does the answered one
 * receive its agent reply - which is also the order the real loop runs in.
 */
export async function seedReview(
  connection: Connection,
  slot: SessionSlot,
  branch: string,
): Promise<string> {
  if (!import.meta.env.DEV) return 'not a development build';

  const seeded = SEEDS.map((seed) => ({ ...seed, id: crypto.randomUUID() }));
  const shape = (
    seed: (typeof seeded)[number],
    status: string,
    turns: { author: 'User' | 'Agent'; text: string }[],
    reviewId: string | null,
  ) => ({
    id: seed.id,
    anchor: {
      path: seed.path,
      side: seed.side,
      line: seed.line,
      content_hash: 0,
      context: seed.context,
      base_ref: 'main',
    },
    comments: turns.map((turn) => ({
      author: turn.author === 'User' ? 'User' : { Agent: { label: AGENT } },
      text: turn.text,
      at: '',
      review_id: turn.author === 'User' ? reviewId : null,
    })),
    status,
    created_at: '',
    updated_at: '',
    commit: null,
  });

  // Phase 1: each thread carries only its reviewer turn, unfiled; the round
  // then seals those turns. The answered one starts Open so the seal does not
  // have to flip it, and the rest keep the state they were seeded for.
  const threads = seeded.map((seed) =>
    shape(
      seed,
      seed.status === 'Addressed' ? 'Open' : seed.status,
      seed.turns.filter((turn) => turn.author === 'User'),
      null,
    ),
  );
  for (const thread of threads) {
    await connection.dispatch(
      { upsert_review_thread: { project: slot.project, branch, thread } },
      slot,
    );
  }
  const filed = await connection.dispatch(
    {
      submit_review: {
        project: slot.project,
        branch,
        summary: SEED_MARKER + ': one addressed, one open, one outdated.',
        thread_ids: threads.map((thread) => thread.id),
        origin: slot,
      },
    },
    slot,
  );
  const number =
    filed !== null && typeof filed === 'object' && 'number' in filed
      ? (filed as { number: number }).number
      : null;
  const reviewId =
    filed !== null && typeof filed === 'object' && 'id' in filed ? String(filed.id) : null;

  // Phase 2: the answered thread gets its agent reply, its reviewer turn now
  // carrying the round, and the state the reply puts it in.
  const answered = seeded.find((seed) => seed.status === 'Addressed');
  if (answered !== undefined && reviewId !== null) {
    await connection.dispatch(
      {
        upsert_review_thread: {
          project: slot.project,
          branch,
          thread: shape(answered, 'Addressed', answered.turns, reviewId),
        },
      },
      slot,
    );
  }

  // The record's reviews ride the snapshot, so a re-read is what carries the
  // seeded round onto the page: nothing pushes a review change.
  connection.refresh({ session: slot });

  return number === null ? 'filed a round' : `filed round #${number}`;
}
