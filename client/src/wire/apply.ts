/**
 * One subject's stream, applied against the version its store holds.
 *
 * **Both transports share this code, not a copy of it.** The page dials the
 * socket itself in a browser and the desktop shell carries the connection in
 * Rust; the frames arrive by two doors, and a version rule kept twice is a
 * version rule that drifts. So each transport brings only what is its own -
 * the stores it holds, how a whole read is asked for, and where a report goes
 * - and the placing rule lives here once.
 *
 * The rules it owns are the ones the wire contract states:
 *
 * - a snapshot is the whole read: it seeds the store and the version the next
 *   patch is placed against, and it ends whatever gap the subject waited on;
 * - a patch applies only as the next number, and anything else - no read yet,
 *   a skipped number, a record patch with no number, a patch this client
 *   cannot read - is a gap: the store keeps its record, the patch is held as
 *   unexpected so a page draws it as itself (rule 25), and one whole read is
 *   asked for;
 * - a second gap while that read is in flight rides the same ask, so a stream
 *   of frames can never become a poll.
 */

import { subjectKey, type Subject } from '../protocol';
import type { Store } from '../stores';
import { applyPatchFor } from './patch';

/** What a transport tells this module about its own connection. */
export interface StreamHost {
  /** The store a subject is held under, or `undefined` when none was read. */
  store(subject: Subject): Store | undefined;
  /**
   * Ask for one whole read of the subject.
   *
   * `false` when the connection cannot carry the ask - a dropped socket reads
   * nothing, because its reconnect re-subscribes every held subject and that
   * answer is the read the gap wanted.
   */
  refresh(subject: Subject): boolean;
  /** Say what could not be placed, with the payload, rather than dropping it. */
  report(why: string, data?: unknown): void;
}

/** The stream state one connection keeps for its subjects. */
export class SubjectStreams {
  /**
   * Subjects waiting on the one whole read a version gap earns.
   *
   * **One read per gap, never a loop.** A second gap while that read is in
   * flight is the same fault and rides the same ask; only a snapshot (or the
   * subject ending) clears the entry, so the next real gap asks again.
   */
  private readonly resyncing = new Set<string>();

  constructor(private readonly host: StreamHost) {}

  /** A whole read: the store takes it, and any gap it waited on ends. */
  snapshot(subject: Subject, version: unknown, data: unknown): void {
    this.resyncing.delete(subjectKey(subject));
    this.host.store(subject)?.set(data, typeof version === 'number' ? version : undefined);
  }

  /** One patch, placed against the version its store holds. */
  patch(subject: Subject, version: unknown, data: unknown): void {
    const store = this.host.store(subject);
    const key = subjectKey(subject);
    if (store === undefined) {
      // **A patch for a subject this connection never read is a bug on one
      // side or the other.** There is no store to draw it in, so it is
      // reported with its payload rather than vanishing.
      this.host.report(`a patch arrived for ${key}, which this connection never read`, data);
      return;
    }
    const arrived = typeof version === 'number' ? version : null;
    const seat = typeof subject !== 'string';
    const held = store.version();
    if (arrived === null && !seat) {
      // A record subject's patch has no place without its number.
      this.gap(subject, store, 'an unversioned patch');
      store.unplaced(data);
      return;
    }
    if (arrived !== null && (held === null || arrived !== held + 1)) {
      this.gap(
        subject,
        store,
        held === null ? `version ${arrived} before any read` : `version ${arrived} over ${held}`,
      );
      store.unplaced(data);
      return;
    }
    const applied = applyPatchFor(subject, store.snapshot(), data);
    if (applied === null) {
      this.gap(subject, store, 'a patch this client could not read');
      store.unplaced(data);
      return;
    }
    store.applied(arrived, applied);
  }

  /**
   * A subject is over on this connection - refused or released.
   *
   * **A gap must not outlive the subject it belonged to.** Left behind, its
   * entry says a whole read is still in flight for a subject that will never
   * answer, and the next real gap's ask is swallowed for the life of the
   * connection.
   */
  end(subject: Subject): void {
    this.resyncing.delete(subjectKey(subject));
  }

  /**
   * The connection dropped: every gap it was waiting on is over.
   *
   * The reconnect re-subscribes every held subject, and that answer is the
   * whole read each gap wanted, so a waiting entry would only suppress the
   * next real gap's ask.
   */
  reset(): void {
    this.resyncing.clear();
  }

  /** One whole read for a subject whose stream skipped, bounded to one at a time. */
  private gap(subject: Subject, store: Store, why: string): void {
    store.gap(why);
    const key = subjectKey(subject);
    if (this.resyncing.has(key)) return;
    this.resyncing.add(key);
    this.host.report(`the ${key} stream skipped a version; asking for one whole read`, { why });
    if (!this.host.refresh(subject)) this.resyncing.delete(key);
  }
}
