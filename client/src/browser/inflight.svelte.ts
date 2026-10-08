/**
 * Whether a browser call is running, for the strip's live mark.
 *
 * The state is about the CLIENT, not a session: every ask the client answers
 * passes through `hostTheBrowser`, so the counter lives beside that wrapper
 * and the strip reads it for the same ring the conversation draws for work in
 * flight.
 *
 * **`visible` carries a short tail past the last call.** Calls arrive in
 * bursts of tens of milliseconds, and a mark that blinks per call reads as a
 * stutter rather than as work; the ring stays up for the tail so a burst
 * draws as one working stretch. `calls` itself stays exact.
 */
export const browserInflight = $state({ calls: 0, visible: false });

/** How long the ring stays up after the last call lands, in ms. */
const TAIL_MS = 400;

let shownAt = 0;
let tail: ReturnType<typeof setTimeout> | null = null;

/** A call started: the ring is up, and any pending tail is cancelled. */
export function callUp(): void {
  browserInflight.calls += 1;
  if (!browserInflight.visible) {
    browserInflight.visible = true;
    shownAt = Date.now();
  }
  if (tail !== null) {
    clearTimeout(tail);
    tail = null;
  }
}

/** A call landed: the ring lives out the tail, so a burst reads as one. */
export function callDown(): void {
  browserInflight.calls -= 1;
  if (browserInflight.calls > 0) return;
  const left = TAIL_MS - (Date.now() - shownAt);
  if (left <= 0) {
    browserInflight.visible = false;
    return;
  }
  tail = setTimeout(() => {
    tail = null;
    if (browserInflight.calls === 0) browserInflight.visible = false;
  }, left);
}
