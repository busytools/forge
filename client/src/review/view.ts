/**
 * What the review page derives from a seat's review reads: the loop's state,
 * a round's tally, the turns of a thread. All pure - the page draws what
 * these return and holds nothing of its own.
 *
 * The wire's spellings cross as they are; every derivation here reads the
 * narrowed record, and a state this build cannot name is carried through as
 * its own word rather than rounded to a neighbour.
 */

import type {
  ReviewAuthorRecord,
  ReviewSetRecord,
  ReviewStatusRecord,
  ReviewThreadRecord,
} from '../session/wire';
import { elapsedLabel } from '../home/view';

/** A state's own word: this build's lowercase spelling, or the wire's. */
export function statusName(status: ReviewStatusRecord): string {
  return typeof status === 'string' ? status : status.unknown;
}

/** Who a turn is from: the reviewer reads as `you`, an agent by its label. */
export function authorName(author: ReviewAuthorRecord): string {
  if (author.kind === 'user') return 'you';
  if (author.kind === 'agent') return author.label;
  return 'unknown';
}

/**
 * Whether a thread awaits the reviewer: the agent answered and nobody has
 * acted since.
 *
 * `open` never awaits even when an agent turn is last - that state is only
 * reached by a reopen or by code returning, both the reviewer handing the
 * work back - and a reviewer turn after the answer puts the ball back with
 * the agent.
 */
export function awaitsReviewer(thread: ReviewThreadRecord): boolean {
  if (thread.status !== 'addressed' && thread.status !== 'outdated') return false;
  const last = thread.comments[thread.comments.length - 1];
  return last !== undefined && last.author.kind !== 'user';
}

/** The loop's state as the page header states it. */
export interface LoopState {
  waiting: number;
  open: number;
}

export function loopState(threads: ReviewThreadRecord[]): LoopState {
  return {
    waiting: threads.filter(awaitsReviewer).length,
    open: threads.filter((thread) => thread.status === 'open').length,
  };
}

/** One round's tally: its member threads, by state. */
export interface RoundTally {
  total: number;
  open: number;
  addressed: number;
  resolved: number;
  outdated: number;
  /** Member threads whose state this build cannot name. */
  named: number;
}

/** The threads a round's membership covers: any turn filed into it. */
export function threadsInRound(
  threads: ReviewThreadRecord[],
  reviewId: string,
): ReviewThreadRecord[] {
  return threads.filter((thread) => thread.comments.some((turn) => turn.review_id === reviewId));
}

export function roundTally(threads: ReviewThreadRecord[], review: ReviewSetRecord): RoundTally {
  const tally: RoundTally = {
    total: 0,
    open: 0,
    addressed: 0,
    resolved: 0,
    outdated: 0,
    named: 0,
  };
  for (const thread of threadsInRound(threads, review.id)) {
    tally.total += 1;
    if (thread.status === 'open') tally.open += 1;
    else if (thread.status === 'addressed') tally.addressed += 1;
    else if (thread.status === 'resolved') tally.resolved += 1;
    else if (thread.status === 'outdated') tally.outdated += 1;
    else tally.named += 1;
  }
  return tally;
}

/** The round a turn was sealed into, by the review it names - for the `R#`
 *  tag on a turn. `null` for a turn no review has sealed. */
export function turnRound(
  reviews: ReviewSetRecord[],
  turn: { review_id: string | null },
): number | null {
  if (turn.review_id === null) return null;
  return reviews.find((review) => review.id === turn.review_id)?.number ?? null;
}

/** The rounds newest first, which is the order every surface lists them in. */
export function roundsNewestFirst(reviews: ReviewSetRecord[]): ReviewSetRecord[] {
  return [...reviews].sort((a, b) => b.number - a.number);
}

/**
 * How long ago a review's rfc3339 stamp was, in the fleet's own short form.
 * An unparsable stamp draws nothing rather than a confident zero.
 */
export function ageOf(at: string, now: number): string {
  const seconds = Math.floor(Date.parse(at) / 1000);
  if (!Number.isFinite(seconds)) return '';
  return elapsedLabel({ secs_since_epoch: seconds, nanos_since_epoch: 0 }, now);
}
