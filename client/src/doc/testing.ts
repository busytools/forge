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
  | { kind: 'landed'; document_kind: DocumentKind; bytes: Uint8Array; reviewable?: boolean }
  | { kind: 'refused'; why: string }
  | { kind: 'silent' };

export interface FakeDoc {
  connection: DocumentConnection;
  /** Every read asked for, in order. */
  reads: { slot: SessionSlot; path: string; reviewable: boolean }[];
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
  const emit = (message: ServerMessage): void => {
    for (const fn of [...listeners]) fn(message);
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
  } as unknown as DocumentConnection;
  return { connection, reads, emit };
}

/** Bytes as the wire carries them. */
function base64Of(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}
