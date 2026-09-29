/**
 * One store per subscription, holding the snapshot a subject was answered
 * with and the updates that followed it.
 *
 * **A store holds both**, because a view redraws from either: a snapshot is
 * the whole subject and an update is one change to it, and which of the two a
 * page reads is the page's business.
 *
 * `svelte/store` rather than runes, because these are created per connection
 * and outlive any component: a rune outside a component needs a reactive root
 * and this has none.
 */

import { get, writable, type Readable } from 'svelte/store';

import type { Subject, SessionUpdate } from './protocol';
import { subjectKey } from './protocol';

export interface Store {
  /** The subject this store is held under, as it was subscribed. */
  subject: Subject;
  /** A view subscribes to redraw; the value changes with the snapshot and the updates. */
  readonly value: Readable<StoreValue>;
  snapshot(): unknown;
  updates(): SessionUpdate[];
  /** Replaces the snapshot and drops the updates it supersedes. */
  set(snapshot: unknown): void;
  push(update: SessionUpdate): void;
}

/** What a `$store` read gives a component: both halves in one value. */
export interface StoreValue {
  snapshot: unknown;
  updates: SessionUpdate[];
}

function createStore(subject: Subject): Store {
  const inner = writable<StoreValue>({ snapshot: null, updates: [] });
  return {
    subject,
    value: { subscribe: inner.subscribe },
    snapshot: () => get(inner).snapshot,
    updates: () => get(inner).updates,
    set: (snapshot) => inner.set({ snapshot, updates: [] }),
    push: (update) =>
      inner.update((held) => ({ snapshot: held.snapshot, updates: [...held.updates, update] })),
  };
}

/** The stores one connection holds, by subject. */
export class Stores {
  private readonly held = new Map<string, { store: Store; subscriptions: number }>();

  /**
   * A store for a subject, adding a subscription to it.
   *
   * Counted rather than replaced: a second subscribe to one subject is a
   * second subscription, and one unsubscribe must not take the seat out of
   * the set a view is still watching.
   */
  open(subject: Subject): Store {
    const key = subjectKey(subject);
    const existing = this.held.get(key);
    if (existing) {
      existing.subscriptions += 1;
      return existing.store;
    }
    const store = createStore(subject);
    this.held.set(key, { store, subscriptions: 1 });
    return store;
  }

  /**
   * One subscription to a subject, dropped. The store goes with the last
   * one, and answers `true` when it did.
   */
  close(subject: Subject): boolean {
    const key = subjectKey(subject);
    const existing = this.held.get(key);
    if (!existing) return false;
    existing.subscriptions -= 1;
    if (existing.subscriptions > 0) return false;
    this.held.delete(key);
    return true;
  }

  get(subject: Subject): Store | undefined {
    return this.held.get(subjectKey(subject))?.store;
  }
}
