/**
 * One store per subscription, holding the snapshot a subject was answered
 * with and the updates that followed it.
 *
 * **A store holds both**, because a view redraws from either: a snapshot is
 * the whole subject and an update is one change to it, and which of the two a
 * page reads is the page's business.
 *
 * **A seat's store is keyed by the seat and holds one occupant's content**, so
 * a replacement drops what is there rather than leaving the next reader to
 * seed from the occupant that left (`REPLACES`). The key stays the seat
 * because the subscription does: it is counted, re-asked on a reconnect, and
 * routed to by the slot an update carries.
 *
 * **It also holds where the subscription got to**, because a refused
 * subscribe is an answer rather than a silence - the server sends one for a
 * seat nobody has started, so a page can say why instead of drawing an empty
 * snapshot as a broken page. `null` for `snapshot` cannot carry that: it
 * means loading, refused and genuinely empty at once.
 *
 * `svelte/store` rather than runes, because these are created per connection
 * and outlive any component: a rune outside a component needs a reactive root
 * and this has none.
 */

import { get, writable, type Readable } from 'svelte/store';

import type { Subject, SessionUpdate } from './protocol';
import { subjectKey } from './protocol';
import { REPLACES, variantOf } from './session/apply';

/**
 * How many updates a store keeps before the oldest are dropped.
 *
 * A store holds what a page has not folded yet, and a page folds as it goes.
 * A connection stays open for a working day, so without a ceiling a busy seat
 * would grow this list for as long as the app is running.
 */
const MAX_UPDATES = 500;

/** Where a subscription got to. */
export type StoreState =
  /** Subscribed, and nothing has answered yet. */
  | { kind: 'loading' }
  /** Answered with a snapshot. */
  | { kind: 'ready' }
  /** Refused, with the server's own reason. */
  | { kind: 'refused'; why: string };

/** What a `$store` read gives a component. */
export interface StoreValue {
  snapshot: unknown;
  updates: SessionUpdate[];
  state: StoreState;
  /**
   * How many updates the ceiling dropped off the front of `updates`, since
   * the snapshot it holds. A page folding them needs to know its tail is not
   * the whole story rather than finding out by arithmetic that does not add
   * up.
   */
  dropped: number;
}

export interface Store {
  /** The subject this store is held under, as it was subscribed. */
  subject: Subject;
  /** A view subscribes to redraw; the value changes with the snapshot and the updates. */
  readonly value: Readable<StoreValue>;
  snapshot(): unknown;
  updates(): SessionUpdate[];
  state(): StoreState;
  dropped(): number;
  /** Replaces the snapshot and drops the updates it supersedes. */
  set(snapshot: unknown): void;
  /**
   * One update, held for a page to fold.
   *
   * A seat's store drops everything when the update replaces the seat's
   * record (`REPLACES`): what it holds is the previous occupant's, and a
   * reader seeding from it would draw that occupant's answer for the one
   * that arrived, until the seat's own answer lands.
   */
  push(update: SessionUpdate): void;
  /** The server declined this subscription, with its own words for why. */
  refuse(why: string): void;
}

/**
 * Whether a subject is one seat's, rather than the fleet's or the pool's.
 *
 * The rule below is about a seat's record, and the home and usage stores hold
 * subjects of their own: a seat's swap is news for the home, not a reason to
 * drop what the home holds.
 */
function isSeat(subject: Subject): boolean {
  return typeof subject === 'object';
}

/** Whether one update replaces a seat's record rather than patching it. */
function replaces(update: SessionUpdate): boolean {
  const [name] = variantOf(update);
  return name !== null && REPLACES.includes(name);
}

function createStore(subject: Subject, state: StoreState = { kind: 'loading' }): Store {
  const inner = writable<StoreValue>({ snapshot: null, updates: [], state, dropped: 0 });
  return {
    subject,
    value: { subscribe: inner.subscribe },
    snapshot: () => get(inner).snapshot,
    updates: () => get(inner).updates,
    state: () => get(inner).state,
    dropped: () => get(inner).dropped,
    set: (snapshot) => inner.set({ snapshot, updates: [], state: { kind: 'ready' }, dropped: 0 }),
    push: (update) =>
      inner.update((held) => {
        // The seat keeps its key and drops its content: the next reader seeds
        // from nothing rather than from the occupant that left.
        if (isSeat(subject) && replaces(update)) {
          return { snapshot: null, updates: [], state: { kind: 'loading' }, dropped: 0 };
        }
        const updates = [...held.updates, update];
        const over = updates.length - MAX_UPDATES;
        return {
          snapshot: held.snapshot,
          updates: over > 0 ? updates.slice(over) : updates,
          state: held.state,
          dropped: held.dropped + Math.max(0, over),
        };
      }),
    refuse: (why) =>
      inner.update((held) => ({ ...held, updates: [], state: { kind: 'refused', why } })),
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
   * A store that will never be subscribed, for a connection that is already
   * closed. Handing back a live-looking store would promise a snapshot no
   * reconnect would ever bring, because nothing replays a subscribe made
   * after the socket went.
   */
  refused(subject: Subject, why: string): Store {
    return createStore(subject, { kind: 'refused', why });
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

  /**
   * How many subscriptions this connection holds for a subject, which is how
   * many times the server must be asked for it again after a drop: one
   * unsubscribe drops one of its entries, so a reconnect that re-asks once
   * for a subject held twice leaves them out of step.
   */
  count(subject: Subject): number {
    return this.held.get(subjectKey(subject))?.subscriptions ?? 0;
  }

  /** The store held at a `subjectKey`, for a caller that already has the key. */
  byKey(key: string): Store | undefined {
    return this.held.get(key)?.store;
  }
}
