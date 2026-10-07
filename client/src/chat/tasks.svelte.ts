/**
 * The project's tasks, as the strip's row draws them, and the change a row is
 * lit for: a task that moved state, or arrived, since the last sync.
 *
 * The row's read: know that something happened without watching the list -
 * so a change lights the row for a beat and the light clears itself, and the
 * list is what a hover opens.
 */
import { untrack } from 'svelte';
import { SvelteMap, SvelteSet } from 'svelte/reactivity';

import type { TaskStripRow } from '../session/view';

/** How long a changed row stays lit: the reveal flash's own beat. */
const LIT_MS = 6000;

export class Tasks {
  #rows = $state<TaskStripRow[]>([]);
  #lit = new SvelteSet<string>();
  #timers = new SvelteMap<string, ReturnType<typeof setTimeout>>();
  /** Whether a sync has landed since the last null one: the first lights nothing. */
  #synced = false;

  /**
   * Take the project's rows whole, lighting the ones that moved.
   *
   * Only the first sync past a null lights nothing - a page opening is not a
   * change - and after it a task that arrived or moved lights, an arrival
   * into an empty list included: both are the set changing under a reader.
   * A null sync forgets, so a record that comes back opens the page rather
   * than lighting every row as an arrival.
   *
   * **Nothing in the body may register on the effect that calls it.** The
   * page's sync lives in an effect that re-runs on every home frame, and a
   * read of the state this same run writes - or of a light's own map - makes
   * that effect its own dependent, which the depth guard ends the page with.
   * Writes still notify the rows that read them: `untrack` hides the reads,
   * not the tells.
   */
  sync(rows: readonly TaskStripRow[] | null): void {
    untrack(() => {
      const prior = this.#rows;
      const lit = [...this.#lit];
      const next = [...(rows ?? [])];
      this.#rows = next;
      // A light belongs to a row: one that is no longer in the list has
      // nothing to light, and a timer still ticking for it is state nobody
      // can see.
      for (const id of lit) {
        if (next.some((held) => held.id === id)) continue;
        this.#lit.delete(id);
        const held = this.#timers.get(id);
        if (held !== undefined) {
          clearTimeout(held);
          this.#timers.delete(id);
        }
      }
      const opening = !this.#synced;
      this.#synced = rows !== null;
      if (opening) return;
      for (const row of next) {
        const was = prior.find((held) => held.id === row.id)?.status;
        if (was === undefined || was !== row.status) this.#light(row.id);
      }
    });
  }

  #light(id: string): void {
    this.#lit.add(id);
    const held = this.#timers.get(id);
    if (held !== undefined) clearTimeout(held);
    this.#timers.set(
      id,
      setTimeout(() => {
        this.#lit.delete(id);
        this.#timers.delete(id);
      }, LIT_MS),
    );
  }

  /** Whether this row has moved since a beat ago. */
  lit(id: string): boolean {
    return this.#lit.has(id);
  }

  /** How much of the set is done. */
  count(): { done: number; total: number } {
    return {
      done: this.#rows.filter((row) => row.status === 'completed').length,
      total: this.#rows.length,
    };
  }

  anything(): boolean {
    return this.#rows.length > 0;
  }

  rows(): readonly TaskStripRow[] {
    return this.#rows;
  }
}

/** The one project's rows, as the home last stated them. */
export const tasks = new Tasks();
