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
 *
 * Only an `agent` turn counts, which is what the server's own predicate
 * counts: an author shape this build cannot name is drawn, never counted, so
 * this page's tally cannot disagree with every other surface's.
 */
export function awaitsReviewer(thread: ReviewThreadRecord): boolean {
  if (thread.status !== 'addressed' && thread.status !== 'outdated') return false;
  const last = thread.comments[thread.comments.length - 1];
  return last !== undefined && last.author.kind === 'agent';
}

/** The loop's state as the page header states it. */
interface LoopState {
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
interface RoundTally {
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

/**
 * The round a reopen is about: the most recent review the thread has a turn
 * in - the exchange the reviewer is unhappy with, never the origin, which
 * would hand the worker the wrong round. `null` while nothing is filed, or
 * when the named round is not among the ones read.
 */
export function latestRound(reviews: ReviewSetRecord[], thread: ReviewThreadRecord): number | null {
  const turn = thread.comments.findLast((comment) => comment.review_id !== null);
  if (turn === undefined) return null;
  return turnRound(reviews, turn);
}

/**
 * Whether a round has anything in it to act on: at least one of the named
 * threads is neither resolved nor outdated - the predicate the view this
 * client replaced sealed by. A round of resolved comments is the reviewer's
 * own housekeeping, and a worker sent to read it spends a turn learning
 * there is nothing to do. A name the read does not carry cannot be shown
 * settled, so it counts as work.
 */
export function roundHasWork(threads: ReviewThreadRecord[], ids: Iterable<string>): boolean {
  const byId = new Map(threads.map((thread) => [thread.id, thread]));
  for (const id of ids) {
    const thread = byId.get(id);
    if (thread === undefined) return true;
    const name = statusName(thread.status);
    if (name !== 'resolved' && name !== 'outdated') return true;
  }
  return false;
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

/** One canvas placement: the line to draw the thread against, and the
 *  recorded line it moved from when it is not where it was written. */
interface Placed {
  line: number;
  movedFrom: number | null;
}

/**
 * Where a thread draws in one scope, or `null` when it cannot be drawn
 * there.
 *
 * **A commit's own thread sits at its recorded line there**: a commit's
 * diff is immutable, so the numbering it was written in is the numbering
 * it drew in. **In the all-changes scope every thread sits where the
 * server's resolution says**, whichever scope it was written in - a thread
 * written inside a commit draws there too when the branch's diff still
 * carries its line (the server resolves it by content and neighbours,
 * never by its commit-numbered line) - and a thread with no resolution, or
 * one the resolver could not place, cannot be drawn rather than being
 * guessed onto its recorded line.
 *
 * **A commit's scope draws only its own threads.** An all-changes thread's
 * numbering is the branch's, and the record carries no landing for it
 * against a commit's diff, so placing it there would be a guess the
 * resolver refuses to make.
 */
export function canvasLine(thread: ReviewThreadRecord, commit: string | null): Placed | null {
  if (thread.commit !== null && thread.commit === commit) {
    return { line: thread.anchor.line, movedFrom: null };
  }
  if (commit !== null) return null;
  const resolution = thread.resolution;
  if (resolution === null || resolution.state !== 'placed') return null;
  return { line: resolution.line, movedFrom: resolution.moved_from };
}

/** The threads a canvas draws in one scope: a commit's sha, or `null` for
 *  all changes. */
export function threadsInScope(
  threads: ReviewThreadRecord[],
  commit: string | null,
): ReviewThreadRecord[] {
  return threads.filter((thread) => canvasLine(thread, commit) !== null);
}

/**
 * The threads the parked section lists: code the all-changes canvas cannot
 * place and that is not resolved.
 *
 * The fresh landing wins over the store's status, because that status was
 * written by whichever view last resolved and cannot say where the code
 * stands now; a resolved thread parks nowhere, the same reopen rule the view
 * this client replaced had, read from the other side. A thread written
 * inside a commit parks here when the branch's diff no longer carries it -
 * it still draws in that commit's own view, where its numbering is
 * settled.
 */
export function outdatedThreads(threads: ReviewThreadRecord[]): ReviewThreadRecord[] {
  return threads.filter(
    (thread) => statusName(thread.status) !== 'resolved' && canvasLine(thread, null) === null,
  );
}

/** Why a parked thread could not be placed, in words; empty when the read
 *  carried no reason, which the card says by saying nothing. */
export function parkedReason(thread: ReviewThreadRecord): string {
  const resolution = thread.resolution;
  if (resolution === null || resolution.state !== 'outdated') return '';
  return typeof resolution.reason === 'string'
    ? 'the code it was written against is gone'
    : `its code matches ${resolution.reason.ambiguous.matches} places`;
}
