/**
 * The seat's own connector subscriptions, as its strip row draws them.
 *
 * Held as the record is: one seat is on screen, so one list, replaced whole
 * when the home moves. The rows are built in `view.ts` from the project's
 * home row and filtered by the seat - ownership is `team_role`, and the lead
 * reads the no-owner set, so a seat shows its own and never another's.
 */
import type { SeatConnectorRow } from '../session/view';

export class Connectors {
  #rows = $state<SeatConnectorRow[]>([]);

  /** Take the seat's rows whole: what the home says is what a row reads. */
  sync(rows: readonly SeatConnectorRow[] | null): void {
    this.#rows = [...(rows ?? [])];
  }

  /** How many subscriptions this seat holds. */
  count(): number {
    return this.#rows.length;
  }

  /** Whether anything is subscribed at all. */
  anything(): boolean {
    return this.#rows.length > 0;
  }

  rows(): readonly SeatConnectorRow[] {
    return this.#rows;
  }
}

/** The one seat's rows, as the home last stated them. */
export const connectors = new Connectors();
