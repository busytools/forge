import type { QueueEnding, QueuedPromptRow } from '../session/wire';
import type { PendingSend } from './echoes.svelte';
import { joinedLine } from './text';

/**
 * The one vocabulary a card's own status draws: the client's word for a press
 * the core has not answered, the core's word for one it holds, and the bound's
 * word for a link that has gone quiet.
 */
export type RowState = 'sending' | 'queued' | 'not_sent';

/** One card the pile draws, whichever half of the queue holds it. */
export interface DrawnRow {
  uuid: string;
  source: string;
  text: string;
  state: RowState;
  /** When the transport accepted the frame, for the 30 s bound; null for the core's own rows. */
  acceptedAt: number | null;
}

/**
 * How long after acceptance the pile says `no word yet`.
 *
 * **Uncertainty, never failure.** A link that is silent rather than closed is
 * the one case no close path can catch, so the card says it has no word yet
 * and offers nothing destructive; the next authoritative read settles it.
 */
export const NO_WORD_AFTER_MS = 30_000;

/**
 * The cards the pile draws: the core's waiting rows, then this client's sends
 * the core has not answered.
 *
 * **One home per message.** A pending send whose uuid the core's queue already
 * lists is not a second card - the core's row took it over - and a refused
 * pending send is not a card at all: it is an ending line, drawn where it was.
 */
export function drawnRows(queued: QueuedPromptRow[], pending: PendingSend[]): DrawnRow[] {
  const listed = new Set(queued.map((row) => row.uuid));
  const out: DrawnRow[] = queued.map((row) => ({
    uuid: row.uuid,
    source: row.source,
    text: row.text,
    state: 'queued',
    acceptedAt: null,
  }));
  for (const held of pending) {
    if (listed.has(held.uuid) || held.state !== 'sending') continue;
    out.push({
      uuid: held.uuid,
      source: held.source,
      text: held.words,
      state: 'sending',
      acceptedAt: held.acceptedAt,
    });
  }
  return out;
}

/** The refused sends drawing an ending line, oldest first. */
export function endingRows(queued: QueuedPromptRow[], pending: PendingSend[]): QueueEnding[] {
  const listed = new Set(queued.map((row) => row.uuid));
  const out: QueueEnding[] = [];
  for (const held of pending) {
    if (listed.has(held.uuid) || held.state !== 'not_sent') continue;
    out.push({ text: held.words, state: 'not_sent', reason: held.reason, source: held.source });
  }
  return out;
}

/** Whether a card has been accepted but heard nothing for the bound's length. */
export function noWordYet(row: DrawnRow, now: number): boolean {
  if (row.state !== 'sending' || row.acceptedAt === null) return false;
  return now - row.acceptedAt >= NO_WORD_AFTER_MS;
}

/**
 * Whether the sender of an ending may be offered a retry.
 *
 * **A person's own send and a cron's are safe to send again** - the words are
 * theirs and nothing ran. A peer's or a connector's are not this view's to
 * re-send: their sender is elsewhere, and a retry here would send them twice.
 */
export function retryable(source: string): boolean {
  return source === 'you' || source === 'cron';
}

/**
 * The one line an ending leaves where the card was, or `null` for an ending
 * that needs no word: a row that was taken, or one the reader cancelled, has
 * said what it had to say by leaving.
 *
 * **The words and the reason both ride the line.** A refused prompt never
 * reaches a turn, so this line is the only place its words exist after the
 * card leaves, and the reason is the core's own answer for why it is not
 * sent. Naming the ending alone lost the words; naming the words alone left
 * the reader asking why.
 */
export function endingLine(
  ended: QueueEnding | null,
  wordsOf: (text: string) => string = (text) => joinedLine(text),
): string | null {
  if (ended === null) return null;
  const words = wordsOf(ended.text);
  const reason = ended.reason === '' ? 'not sent' : `not sent · ${ended.reason}`;
  return words === '' ? reason : `${reason}: ${words}`;
}

/**
 * The pile's own arithmetic, apart from the component so its edges are
 * testable without a DOM: the walk, and where the face sits.
 *
 * A row the core settles leaves `rows` while the cursor may still name it, so
 * both functions take a cursor that can be stale - the walk re-arms at the
 * newest rather than going inert, which is what a walk that trusted the
 * cursor's index did.
 */

/** Where the face sits: the cursor's row, or the newest when the walk is in the box. */
export function faceAt(cursor: string | null, rows: QueuedPromptRow[]): number {
  if (rows.length === 0) return 0;
  if (cursor === null) return rows.length - 1;
  const at = rows.findIndex((row) => row.uuid === cursor);
  return at < 0 ? rows.length - 1 : at;
}

/**
 * Where the cursor lands: up enters at the newest and stops on the first
 * queued prompt, down comes back, and one more down past the newest lands in
 * the box - which is `null`.
 */
export function walk(
  cursor: string | null,
  rows: QueuedPromptRow[],
  direction: 'up' | 'down',
): string | null {
  if (rows.length === 0) return null;
  if (cursor === null) {
    return direction === 'up' ? (rows[rows.length - 1]?.uuid ?? null) : null;
  }
  const at = rows.findIndex((row) => row.uuid === cursor);
  if (at < 0) return null;
  if (direction === 'up') return rows[at - 1]?.uuid ?? cursor;
  return at + 1 >= rows.length ? null : (rows[at + 1]?.uuid ?? null);
}
