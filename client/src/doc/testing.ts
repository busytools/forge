/**
 * The document page's test socket: what a mounted page reads, and the
 * answers a test scripts it with.
 */

import type { DocumentKind, ServerMessage } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';

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

interface FakeDoc {
  connection: Connection;
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
 * The server's commentable set, mirrored from
 * `crates/forge-agent/src/env/documents.rs` (`TEXT_EXTENSIONS`): the double
 * has to answer what the server answers, or a test proves a hand-off
 * behaviour the binary does not have.
 */
const TEXT_EXTENSIONS = new Set([
  'rs',
  'ts',
  'mts',
  'cts',
  'tsx',
  'js',
  'mjs',
  'cjs',
  'jsx',
  'svelte',
  'py',
  'go',
  'json',
  'toml',
  'yaml',
  'yml',
  'sh',
  'bash',
  'zsh',
  'css',
  'html',
  'htm',
  'xml',
  'svg',
  'sql',
  'kt',
  'kts',
  'md',
  'markdown',
  'ini',
  'txt',
]);

/**
 * Whether the server would call this kind reviewable - the read's own
 * answer, so the double says what the server says and not what the caller
 * asked for. A markdown or text document is reviewable whoever opened it;
 * a kind in the extension set is reviewable; a PDF, an image or an
 * extensionless file opens read-only.
 */
function reviewableKind(kind: DocumentKind): boolean {
  if (kind === 'markdown' || kind === 'text') return true;
  return typeof kind === 'object' && TEXT_EXTENSIONS.has(kind.other);
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
    connection: undefined as unknown as Connection,
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
          reviewable: answer.reviewable ?? reviewableKind(answer.document_kind),
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
    // The page reads the greeting's dictation axes for the panel's defaults;
    // a fake with none answers as an install that only set `forge.toml`.
    settings: () => null,
    // A submit answers through a reply, as the server does; everything else
    // the page sends answers the crossing, `null` inside.
    dispatch(command: unknown): Promise<unknown> {
      sent.push(command);
      const variant = Object.keys(command as Record<string, unknown>)[0];
      if (variant !== 'submit_review') return Promise.resolve(null);
      if ('reject' in held.submitAnswer) {
        return Promise.reject(new Error(held.submitAnswer.reject));
      }
      return Promise.resolve(held.submitAnswer.body);
    },
  } as unknown as Connection;
  held.connection = connection;
  return held;
}

/** Bytes as the wire carries them. */
function base64Of(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}
