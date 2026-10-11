/**
 * What the review page writes: the thread JSON its compose and reply paths
 * send, and the anchor capture its comments carry.
 *
 * **The anchor is captured here because the lines are here.** The page
 * draws the bounded content, so the line's text - and its neighbours - are
 * in hand at the moment a comment is written, exactly as the terminal's
 * overlay captures from its own scan. The hash is the crate's FNV-1a over
 * whitespace-collapsed UTF-8, and a thread written with a different hash
 * would never place in the terminal: the three vectors in the test beside
 * this file are pinned in forge-agent's resolver tests too, so changing
 * either implementation alone fails the other's test.
 *
 * The hash crosses as decimal digits rather than a JSON number: JS numbers
 * are doubles, exact only to 2^53, and an FNV-1a hash is a full u64. The
 * anchor's deserializer accepts both forms.
 */

import type { DiffLineRecord, ReviewThreadRecord } from '../session/wire';
import type { SessionSlot } from '../wire/types';

/** How many lines are captured on each side of an anchor, the crate's own
 *  `CONTEXT_RADIUS`. */
const CONTEXT_RADIUS = 3;

const OFFSET = 0xcbf29ce484222325n;
const PRIME = 0x100000001b3n;
const MASK = (1n << 64n) - 1n;

/**
 * FNV-1a over the whitespace-collapsed line, in the line's UTF-8 bytes.
 *
 * `TextEncoder` rather than the string's own code units: the crate hashes
 * `str::as_bytes`, and a line with any non-ASCII character hashes
 * differently over UTF-16 code units. The collapse mirrors Rust's
 * `split_whitespace().join(" ")`, so a `cargo fmt` reflow reads as the
 * same line.
 */
export function anchorHash(line: string): bigint {
  const text = line
    .split(/\s+/u)
    .filter((word) => word !== '')
    .join(' ');
  let hash = OFFSET;
  for (const byte of new TextEncoder().encode(text)) {
    hash = ((hash ^ BigInt(byte)) * PRIME) & MASK;
  }
  return hash;
}

/**
 * Up to `radius` lines on each side of `index`, excluding the line itself,
 * as the crate's `capture_context` takes them: neighbours within the hunk,
 * by index, whatever their kind.
 */
export function captureContext(
  lines: DiffLineRecord[],
  index: number,
  radius = CONTEXT_RADIUS,
): string[] {
  const start = Math.max(0, index - radius);
  const end = Math.min(lines.length, index + radius + 1);
  const captured: string[] = [];
  for (let at = start; at < end; at += 1) {
    if (at === index) continue;
    const line = lines[at];
    if (line !== undefined) captured.push(line.text);
  }
  return captured;
}

/** One thread as the upsert command carries it - the store's own shape. */
export interface ThreadPayload {
  id: string;
  anchor: {
    path: string;
    side: 'Old' | 'New';
    line: number;
    content_hash: string;
    context: string[];
    base_ref: string;
  };
  comments: {
    author: 'User' | { Agent: { label: string } };
    text: string;
    at: string;
    review_id: string | null;
  }[];
  status: string;
  created_at: string;
  updated_at: string;
  commit: string | null;
}

/** What the canvas hands back when a comment is written on a line. */
export interface ComposedComment {
  path: string;
  side: 'old' | 'new';
  line: number;
  text: string;
  context: string[];
  /** The line's own text, which the hash is taken of. */
  lineText: string;
}

export interface NewComment extends ComposedComment {
  baseRef: string;
  /** The scope the comment was written in: a commit's sha, or null for the
   *  branch's own all-changes scope. */
  commit: string | null;
}

/** The store fills what the writer leaves empty - `at`, `created_at`, the
 *  updated stamp - so an empty string is the honest value here, exactly as
 *  the terminal's overlay sends. */
export function newThread(comment: NewComment): ThreadPayload {
  return {
    id: crypto.randomUUID(),
    anchor: {
      path: comment.path,
      side: comment.side === 'old' ? 'Old' : 'New',
      line: comment.line,
      content_hash: anchorHash(comment.lineText).toString(),
      context: comment.context,
      base_ref: comment.baseRef,
    },
    comments: [{ author: 'User', text: comment.text, at: '', review_id: null }],
    status: 'Open',
    created_at: '',
    updated_at: '',
    commit: comment.commit,
  };
}

/**
 * The thread as a reply leaves it: the same thread with the reviewer's turn
 * appended, unfiled. Unfiled is the point - the round that seals it is the
 * one `submit_review` writes, and until then the turn is a draft the page
 * owes a submit.
 */
export function withReply(thread: ReviewThreadRecord, text: string): ThreadPayload {
  return {
    id: thread.id,
    anchor: {
      path: thread.anchor.path,
      side: thread.anchor.side === 'old' ? 'Old' : 'New',
      line: thread.anchor.line,
      // The stored hash is already digits on this side - the record carried
      // it as a string, so echoing it back is exact.
      content_hash: thread.anchor.content_hash,
      context: thread.anchor.context,
      base_ref: thread.anchor.base_ref,
    },
    comments: [
      ...thread.comments.map((turn) => ({
        author:
          turn.author.kind === 'agent'
            ? ({ Agent: { label: turn.author.label } } as const)
            : ('User' as const),
        text: turn.text,
        at: turn.at,
        review_id: turn.review_id,
      })),
      { author: 'User' as const, text, at: '', review_id: null },
    ],
    status:
      thread.status === 'open'
        ? 'Open'
        : thread.status === 'addressed'
          ? 'Addressed'
          : thread.status === 'resolved'
            ? 'Resolved'
            : thread.status === 'outdated'
              ? 'Outdated'
              : 'Open',
    created_at: thread.created_at,
    updated_at: thread.updated_at,
    commit: thread.commit,
  };
}

/** The upsert command, as the socket carries it. */
export function upsertCommand(slot: SessionSlot, branch: string, thread: ThreadPayload) {
  return { upsert_review_thread: { project: slot.project, branch, thread } };
}

/** The submit command sealing a round of the listed threads. */
export function submitCommand(
  slot: SessionSlot,
  branch: string,
  threadIds: string[],
  summary: string | null,
) {
  return {
    submit_review: { project: slot.project, branch, summary, thread_ids: threadIds, origin: slot },
  };
}

/** The resolve (or reopen) command for one thread. */
export function statusCommand(
  slot: SessionSlot,
  branch: string,
  threadId: string,
  status: 'Resolved' | 'Open',
) {
  return {
    set_review_thread_status: { project: slot.project, branch, thread_id: threadId, status },
  };
}
