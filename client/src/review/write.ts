/**
 * What the review page writes: the thread JSON its compose and reply paths
 * send, the seal-and-nudge its finish sends, and the anchor capture its
 * comments carry.
 *
 * **The anchor is captured here because the lines are here.** The page
 * draws the bounded content, so the line's text - and its neighbours - are
 * in hand at the moment a comment is written, exactly as the view this
 * client replaced captured from its own scan. The hash is the crate's FNV-1a
 * over whitespace-collapsed UTF-8, and a thread written with a different
 * hash would never place: the vectors in the test beside this file are
 * pinned in forge-agent's resolver tests too, so changing either
 * implementation alone fails the other's test.
 *
 * The hash crosses as decimal digits rather than a JSON number: JS numbers
 * are doubles, exact only to 2^53, and an FNV-1a hash is a full u64. The
 * anchor's deserializer accepts both forms.
 *
 * **Nothing is rounded to a neighbour on the way out.** A state or an
 * author this build cannot name crosses back as the word or the shape it
 * arrived as; a server that does not know it refuses the write loudly,
 * which is the outcome the page draws - a quietly rewritten `Open` would
 * file a lie the reviewer never sees.
 */

import type { DiffLineRecord, ReviewStatusRecord, ReviewThreadRecord } from '../session/wire';
import { mintPromptId } from '../wire/ids';
import type { SessionSlot } from '../wire/types';

/** How many lines are captured on each side of an anchor, the crate's own
 *  `CONTEXT_RADIUS`. */
const CONTEXT_RADIUS = 3;

/** Where a comment box is open: the line of the drawn diff its words belong to. */
export interface ReviewSpot {
  path: string;
  side: 'old' | 'new';
  line: number;
}

const OFFSET = 0xcbf29ce484222325n;
const PRIME = 0x100000001b3n;
const MASK = (1n << 64n) - 1n;

/**
 * The whitespace Rust's `split_whitespace` splits on, spelled out because
 * JS's own `\s` is not the same set: it misses NEL (which Unicode calls
 * White_Space) and includes the BOM (which it does not) - two codepoints
 * that would collapse on one side of the wire and not the other. Both are
 * pinned as vectors in the test beside this file and in the crate's
 * resolver tests.
 *
 * Built from codepoint numbers rather than a regex literal: the literals
 * are invisible in an editor, and an escape spelling does not survive this
 * file's own tooling.
 */
const WHITESPACE = new RegExp(
  '[\\t\\n\\v\\f\\r ' +
    [
      String.fromCodePoint(0x85),
      String.fromCodePoint(0xa0),
      String.fromCodePoint(0x1680),
      `${String.fromCodePoint(0x2000)}-${String.fromCodePoint(0x200a)}`,
      String.fromCodePoint(0x2028),
      String.fromCodePoint(0x2029),
      String.fromCodePoint(0x202f),
      String.fromCodePoint(0x205f),
      String.fromCodePoint(0x3000),
    ].join('') +
    ']+',
  'u',
);

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
    .split(WHITESPACE)
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
    /** `User`, an agent's label, or a shape this build cannot name carried
     *  back verbatim (see the header). */
    author: unknown;
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

interface NewComment extends ComposedComment {
  baseRef: string;
  /** The scope the comment was written in: a commit's sha, or null for the
   *  branch's own all-changes scope. */
  commit: string | null;
}

/** The wire's own word for a state: this build's lowercase spelling
 *  capitalised, or the word an unknown state arrived with. */
function statusWord(status: ReviewStatusRecord): string {
  if (typeof status === 'string') return status.charAt(0).toUpperCase() + status.slice(1);
  return status.unknown;
}

/** A turn's author as the command carries it: the two known shapes, or the
 *  raw value a record this build cannot name held. */
function authorPayload(author: ReviewThreadRecord['comments'][number]['author']): unknown {
  if (author.kind === 'user') return 'User';
  if (author.kind === 'agent') return { Agent: { label: author.label } };
  return author.raw;
}

/** The store fills what the writer leaves empty - `at`, `created_at`, the
 *  updated stamp - so an empty string is the honest value here, exactly as
 *  the view this client replaced sent. */
export function newThread(comment: NewComment): ThreadPayload {
  return {
    id: mintPromptId(),
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
        author: authorPayload(turn.author),
        text: turn.text,
        at: turn.at,
        review_id: turn.review_id,
      })),
      { author: 'User' as const, text, at: '', review_id: null },
    ],
    status: statusWord(thread.status),
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

/** The resolve (or reopen) command for one thread; `Outdated` is the
 *  document viewer aging a comment against a changed document. */
export function statusCommand(
  slot: SessionSlot,
  branch: string,
  threadId: string,
  status: 'Resolved' | 'Open' | 'Outdated',
) {
  return {
    set_review_thread_status: { project: slot.project, branch, thread_id: threadId, status },
  };
}

/** What one send answers with: whether the command went, and the reply's own
 *  body for the commands that answer through one. */
export interface SendOutcome {
  /** `false` only when the command never left - a closed socket, a seat with
   *  no branch - which is also the only case the page draws a refusal for. */
  went: boolean;
  body: unknown;
}

/** The page's two ways to the socket: the `write` whose outcome is drawn
 *  where the reader acted, and the raw `dispatch` a nudge rides. The two are
 *  different on purpose - a write's failure is the reader's to see, and a
 *  nudge's is not. */
export interface Sends {
  write: (command: unknown) => Promise<SendOutcome>;
  /** The connection's own dispatch: `null` for a fire-and-forget command,
   *  and a throw when the socket is not open. */
  dispatch: (command: unknown) => unknown;
}

/** The number a landed submit's reply carries, or `null` when the reply was
 *  a word rather than a round. */
function filedNumber(body: unknown): number | null {
  if (typeof body !== 'object' || body === null) return null;
  const number = (body as { number?: unknown }).number;
  return typeof number === 'number' ? number : null;
}

/** A nudge as the socket carries it: a prompt into the seat's own chat, so
 *  the worker reads the line the view this client replaced sent. */
function nudge(slot: SessionSlot, text: string) {
  return {
    prompt_under: {
      key: slot,
      text,
      attachments: [],
      uuid: mintPromptId(),
      source: 'you',
    },
  };
}

/** Send one nudge, best-effort: the round or the flip it follows is filed
 *  either way, and a nudge that could not leave is the worker's own line
 *  missing, never a lost review - the document viewer's nudge swallows the
 *  same failure for the same reason. */
function sendNudge(sends: Sends, slot: SessionSlot, text: string): void {
  try {
    void sends.dispatch(nudge(slot, text));
  } catch {
    // A closed socket has no seat left to tell; what it lost is one line.
  }
}

/**
 * The finish: seal the drafted threads into a round through `sends.write`,
 * then nudge the seat with the line the round's own number names.
 *
 * **Only a round worth reading is worth a nudge.** `hasWork` is the page's
 * reading of the sealed threads - at least one not resolved or outdated - so
 * a round of nothing but resolved comments files without spending the
 * worker's turn on it; the view this client replaced sealed by the same
 * predicate.
 *
 * **A reply that carried no round is a finish that did not land.** The store
 * turned the submit down, so nothing was filed: no nudge goes out and
 * `false` comes back, which is what keeps the reader's words. A refusal and
 * a write that never left take the same answer.
 */
export async function finishRound(
  sends: Sends,
  slot: SessionSlot,
  branch: string,
  threadIds: string[],
  summary: string | null,
  hasWork: boolean,
): Promise<boolean> {
  const sealed = await sends.write(submitCommand(slot, branch, threadIds, summary));
  if (!sealed.went) return false;
  const number = filedNumber(sealed.body);
  if (number === null) return false;
  if (hasWork) {
    sendNudge(
      sends,
      slot,
      `Review #${number} ready - address it via the review MCP (\`review__list\`).`,
    );
  }
  return true;
}

/**
 * The reopen: flip the thread back to open through `sends.write`, then nudge
 * the seat with the round the reopen is about.
 *
 * The round is the thread's LATEST filed one, and a thread no round has
 * filed reopens under the round-less line; a flip that never left nudges
 * nobody, because the thread was not reopened.
 */
export async function reopenThread(
  sends: Sends,
  slot: SessionSlot,
  branch: string,
  threadId: string,
  round: number | null,
): Promise<boolean> {
  const flipped = await sends.write(statusCommand(slot, branch, threadId, 'Open'));
  if (!flipped.went) return false;
  const line =
    round === null
      ? 'Reopened a review comment - take another look via the review MCP (`review__list`).'
      : `Reopened a comment in review #${round} - take another look via the review MCP (\`review__get\`).`;
  sendNudge(sends, slot, line);
  return true;
}
