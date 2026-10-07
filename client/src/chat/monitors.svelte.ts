/**
 * The session's monitors, as the seat's strip row draws them.
 *
 * Held as the record is: one seat is on screen, so one list, replaced whole
 * when the record moves. A monitor carries its own state - running, or the
 * way it ended - so a stale list would draw a settled monitor as a live one.
 */
import type { MonitorStripRow } from '../session/view';

export class Monitors {
  #rows = $state<MonitorStripRow[]>([]);

  /** Take the record's rows whole: what the wire says is what a row reads. */
  sync(rows: readonly MonitorStripRow[] | null): void {
    this.#rows = [...(rows ?? [])];
  }

  /** How many monitors the session holds. */
  count(): number {
    return this.#rows.length;
  }

  anything(): boolean {
    return this.#rows.length > 0;
  }

  rows(): readonly MonitorStripRow[] {
    return this.#rows;
  }
}

/** The one session's rows, as its record last stated them. */
export const monitors = new Monitors();
