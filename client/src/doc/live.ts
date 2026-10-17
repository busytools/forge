/**
 * The document viewer over a live connection: the read one route makes.
 *
 * **The answer is matched by seat AND document.** A client holding two seats
 * can have two viewers' reads in flight, and the answer names both back, so
 * one seat's document can never draw in another seat's page.
 *
 * **A refused read is drawn, never left loading.** Every refusal - missing,
 * unreadable, over the ceiling, outside the tree, a PDF asked to be
 * reviewable - arrives as an `error` naming `document_read` and its seat,
 * and the server's own sentence is what the page draws.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { bytesOf } from '../browser/host';
import { type Connection, type ConnectionStatus } from '../socket';
import { subjectKey, type DocumentKind, type ServerMessage } from '../protocol';
import {
  reviewSetFrom,
  reviewThreadFrom,
  type ReviewSetRecord,
  type ReviewThreadRecord,
} from '../session/wire';
import type { SessionSlot } from '../wire/types';

/** What watching a document needs of a socket: the read, the frames it is
 *  answered on, and the dispatch a comment and a finish ride. */
export type DocumentConnection = Pick<
  Connection,
  'readDocument' | 'onMessage' | 'onStatus' | 'status' | 'dispatch'
>;

/** What the viewer has to draw. */
export interface DocRead {
  state: 'reading' | 'landed' | 'refused';
  /** The read's answer: the resolved absolute path. */
  path: string | null;
  kind: DocumentKind | null;
  bytes: Uint8Array | null;
  reviewable: boolean;
  /** The document's own hash, as decimal digits - what a thread's anchor
   *  was written against. */
  contentHash: string | null;
  /** The document's comments, and the reviews they were filed into. */
  threads: ReviewThreadRecord[];
  reviews: ReviewSetRecord[];
  /** Why it could not be read: the server's sentence, or the client's own. */
  refused: string | null;
}

const READING: DocRead = {
  state: 'reading',
  path: null,
  kind: null,
  bytes: null,
  reviewable: false,
  contentHash: null,
  threads: [],
  reviews: [],
  refused: null,
};

/** The sentence a read that never went draws, in the client's own words. */
const DOWN = 'the document could not be opened: the connection is down';

/**
 * Whether an answer's resolved path is the one this viewer asked for.
 *
 * The route carries the resolved path (a hand-off raises the read's own
 * answer), so the match is usually exact; a hand-written link may carry a
 * relative one, which the resolved path ends with.
 */
function sameDocument(answered: string, asked: string): boolean {
  // **The answer is canonical, the route need not be**: the server resolves
  // the path, so a route carrying `/tmp/...` is answered `/private/tmp/...`
  // on macOS - and comparing the spellings literally left the page opening
  // for good (measured on the look stack, 2026-10-17).
  const bare = (path: string): string =>
    path.startsWith('/private/') ? path.slice('/private'.length) : path;
  return bare(answered) === bare(asked) || answered.endsWith(`/${asked}`);
}

/**
 * Watch one document: ask the server for it and fold the answer.
 *
 * The listeners are let go with the last subscriber, so a viewer the reader
 * has left stops listening for a document nobody draws.
 */
export function watchDocument(
  connection: DocumentConnection,
  slot: SessionSlot,
  path: string,
  reviewable: boolean,
): Readable<DocRead> {
  const seat = subjectKey({ session: slot });
  let landed = false;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  function ask(): void {
    if (landed) return;
    if (!connection.readDocument(slot, path, reviewable)) {
      view.set({ ...READING, state: 'refused', refused: DOWN });
    }
  }

  function watch(): void {
    landed = false;
    stopMessages = connection.onMessage((message: ServerMessage) => {
      if (message.kind === 'document_read') {
        if (subjectKey({ session: message.session }) !== seat) return;
        if (!sameDocument(message.path, path)) return;
        landed = true;
        view.set({
          state: 'landed',
          path: message.path,
          kind: message.document_kind,
          bytes: bytesOf(message.bytes),
          reviewable: message.reviewable,
          contentHash: message.content_hash,
          threads: message.threads.map(reviewThreadFrom),
          reviews: message.reviews.map(reviewSetFrom),
          refused: null,
        });
        return;
      }
      if (message.kind === 'error' && message.what === 'document_read') {
        const named = message.seat === undefined ? seat : subjectKey({ session: message.seat });
        if (named !== seat) return;
        landed = true;
        view.set({ ...READING, state: 'refused', refused: message.why });
      }
    });
    // A drop takes any read in flight with it; the reopen asks again rather
    // than leaving the page opening for good.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next === 'open') ask();
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
    stopStatus = null;
  }

  const view: Writable<DocRead> = writable(READING, () => {
    watch();
    ask();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}
