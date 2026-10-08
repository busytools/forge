/**
 * Whether a browser call is running, for the strip's live mark.
 *
 * The state is about the CLIENT, not a session: every ask the client answers
 * passes through `hostTheBrowser`, so the counter lives beside that wrapper
 * and the strip reads it for the same ring the conversation draws for work in
 * flight.
 *
 * **`visible` carries a short tail measured from each landing.** Calls arrive
 * in bursts of tens of milliseconds, and a mark that blinks per call reads as
 * a stutter rather than as work; every time the last call lands the tail is
 * re-armed, so a burst of any length draws as one working stretch and only a
 * real pause (longer than the tail) takes the ring down. `calls` itself stays
 * exact.
 */
export const browserInflight = $state({ calls: 0, visible: false });

/** How long the ring stays up after the last call lands, in ms. */
const TAIL_MS = 400;

let tail: ReturnType<typeof setTimeout> | null = null;

/** A call started: the ring is up, and any pending tail is cancelled. */
export function callUp(): void {
  browserInflight.calls += 1;
  browserInflight.visible = true;
  if (tail !== null) {
    clearTimeout(tail);
    tail = null;
  }
}

/** A call landed: the ring lives out the tail from THIS landing, so a burst
 *  stays up as one stretch however long it runs. */
export function callDown(): void {
  browserInflight.calls -= 1;
  if (browserInflight.calls > 0) return;
  if (tail !== null) clearTimeout(tail);
  tail = setTimeout(() => {
    tail = null;
    if (browserInflight.calls === 0) browserInflight.visible = false;
  }, TAIL_MS);
}
