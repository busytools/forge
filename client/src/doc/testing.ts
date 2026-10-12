/**
 * The document page's test socket: what a mounted page reads, and the
 * answers a test scripts it with.
 */

import type { DocumentKind, ServerMessage } from '../protocol';
import type { ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import type { DocumentConnection } from './live';

export const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** What the server answers a read with. */
export type DocAnswer =
  | {
      kind: 'landed';
      document_kind: DocumentKind;
      bytes: Uint8Array;
      reviewable?: boolean;
      content_hash?: string;
      threads?: unknown[];
      reviews?: unknown[];
    }
  | { kind: 'refused'; why: string }
  | { kind: 'silent' };

export interface FakeDoc {
  connection: DocumentConnection;
  /** Every read asked for, in order. */
  reads: { slot: SessionSlot; path: string; reviewable: boolean }[];
  /** Every command dispatched through the page, in order. */
  sent: unknown[];
  /** How the next submit (a reply-carrying command) answers. */
  submitAnswer: { body: unknown } | { reject: string };
  /** Say something to the page, as the server would. */
  emit(message: ServerMessage): void;
}

/**
 * A socket that answers every read with `answer`. The answer is emitted
 * synchronously inside `readDocument`, which is safe because the watcher
 * registers its listener before it asks.
 */
export function fakeDoc(answer: DocAnswer): FakeDoc {
  const listeners = new Set<(message: ServerMessage) => void>();
  const reads: { slot: SessionSlot; path: string; reviewable: boolean }[] = [];
  const sent: unknown[] = [];
  const emit = (message: ServerMessage): void => {
    for (const fn of [...listeners]) fn(message);
  };
  const held: FakeDoc = {
    connection: undefined as unknown as DocumentConnection,
    reads,
    sent,
    emit,
    submitAnswer: { body: { id: 'r1', number: 1, summary: null, created_at: 'x' } },
  };
  const connection = {
    readDocument(slot: SessionSlot, asked: string, reviewable: boolean): boolean {
      reads.push({ slot, path: asked, reviewable });
      if (answer.kind === 'landed') {
        emit({
          kind: 'document_read',
          session: slot,
          path: asked,
          document_kind: answer.document_kind,
          bytes: base64Of(answer.bytes),
          reviewable: answer.reviewable ?? reviewable,
          content_hash: answer.content_hash ?? '0',
          threads: answer.threads ?? [],
          reviews: answer.reviews ?? [],
        });
      } else if (answer.kind === 'refused') {
        emit({ kind: 'error', what: 'document_read', why: answer.why, seat: slot });
      }
      return true;
    },
    onMessage(fn: (message: ServerMessage) => void) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus(_fn: (status: ConnectionStatus) => void) {
      return () => {};
    },
    status: (): ConnectionStatus => 'open',
    // A submit answers through a reply, as the server does; everything else
    // the page sends is fire-and-forget and answers `null`.
    dispatch(command: unknown): Promise<unknown> | null {
      sent.push(command);
      const variant = Object.keys(command as Record<string, unknown>)[0];
      if (variant !== 'submit_review') return null;
      if ('reject' in held.submitAnswer) {
        return Promise.reject(new Error(held.submitAnswer.reject));
      }
      return Promise.resolve(held.submitAnswer.body);
    },
  } as unknown as DocumentConnection;
  held.connection = connection;
  return held;
}

/** Bytes as the wire carries them. */
function base64Of(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}
