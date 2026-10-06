/**
 * The project's schedules, as the seat's strip row draws them.
 *
 * Held as the record is: one seat is on screen, so one list, replaced whole
 * when the home moves or the page's clock ticks - the countdown is part of the
 * row, so a stale list would count toward the past. Crons carry no per-seat
 * owner, so every seat of the project reads the same set.
 */
import type { SeatScheduleRow } from '../session/view';

export class Schedules {
  #rows = $state<SeatScheduleRow[]>([]);

  /** Take the project's rows whole: what the home says is what a row reads. */
  sync(rows: readonly SeatScheduleRow[] | null): void {
    this.#rows = [...(rows ?? [])];
  }

  /** How many schedules the project holds. */
  count(): number {
    return this.#rows.length;
  }

  /** Whether anything is scheduled at all. */
  anything(): boolean {
    return this.#rows.length > 0;
  }

  rows(): readonly SeatScheduleRow[] {
    return this.#rows;
  }
}

/** The one project's rows, as the home and the clock last stated them. */
export const schedules = new Schedules();
