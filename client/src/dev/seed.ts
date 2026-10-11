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

/** File one round on the seat's branch, over its own connection. */
export async function seedReview(
  connection: Connection,
  slot: SessionSlot,
  branch: string,
): Promise<string> {
  if (!import.meta.env.DEV) return 'not a development build';

  const threads = SEEDS.map((seed) => ({
    id: crypto.randomUUID(),
    anchor: {
      path: seed.path,
      side: seed.side,
      line: seed.line,
      content_hash: 0,
      context: seed.context,
      base_ref: 'main',
    },
    comments: seed.turns.map((turn) => ({
      author: turn.author === 'User' ? 'User' : { Agent: { label: AGENT } },
      text: turn.text,
      at: '',
      review_id: null,
    })),
    status: seed.status,
    created_at: '',
    updated_at: '',
    commit: null,
  }));

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

  // The record's reviews ride the snapshot, so a re-read is what carries the
  // seeded round onto the page: nothing pushes a review change.
  connection.refresh({ session: slot });

  const number =
    filed !== null && typeof filed === 'object' && 'number' in filed
      ? (filed as { number: number }).number
      : null;
  return number === null ? 'filed a round' : `filed round #${number}`;
}
