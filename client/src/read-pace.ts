/**
 * The pace a whole-subject read is asked at: one coalesced ask, and never two
 * inside the floor.
 *
 * **A whole read is the only read this wire has.** A subscribe or a refresh
 * is answered with the server's entire encode of the subject - the home's
 * roster, the board's rows - so a page that asks once per covering frame pays
 * a whole encode per frame. The coalescing beat folds a burst, but a fleet
 * that never stops news keeps the beat firing as fast as an answer comes
 * back: on a live fleet with a seat in use, the home re-read 1-3 times a
 * second and the board re-read behind the same frames, each ~195 KB and ~640
 * KB respectively (#3060).
 *
 * **The floor is a glance's staleness, not a cap on freshness.** A row's
 * counts and marks read two seconds late are still the same glance, and the
 * read that goes at the floor is the newest answer there is: the news that
 * arrived meanwhile is asked for then, never dropped.
 *
 * Two seconds halves the traffic at the frame frequency the issue measured.
 * The deeper fix is the frames' own fold - a frame that already carries the
 * subject's data should buy no read at all - and that is a decision about
 * duplicating the server's derivation on the client, made separately.
 */
export const READ_FLOOR_MS = 2_000;

/**
 * A whole-subject read on a coalescing beat, with the floor between reads.
 *
 * `read` answers whether the ask really went: a guard that found a read
 * already in flight, or a transport with no socket, is not a read and does
 * not start the floor - the next ask is free to go at the beat.
 */
export class ReadPace {
  private timer: ReturnType<typeof setTimeout> | null = null;

  /** When the last read went, or `0` for none yet. */
  private lastAt = 0;

  constructor(
    private readonly read: () => boolean,
    private readonly coalesceMs: number,
  ) {}

  /**
   * Ask for a read: at the coalescing beat, or at the floor when one has
   * just gone, whichever is later.
   */
  schedule(): void {
    if (this.timer !== null) return;
    const wait = Math.max(this.coalesceMs, this.lastAt + READ_FLOOR_MS - Date.now());
    this.timer = setTimeout(() => {
      this.timer = null;
      if (!this.read()) return;
      this.lastAt = Date.now();
    }, wait);
  }

  /**
   * A read that went some other way starts the floor.
   *
   * A page's return, a reconnect and a review mount ask directly rather than
   * through the beat; the floor is owed from the moment that ask left, or the
   * beat would buy a second whole encode right behind it.
   */
  went(): void {
    this.lastAt = Date.now();
  }

  /** Drop the armed read: no reader is left for its answer. */
  stop(): void {
    if (this.timer === null) return;
    clearTimeout(this.timer);
    this.timer = null;
  }
}
