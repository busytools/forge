/**
 * The page's heartbeat, one tick a second, off the main thread (#2374).
 *
 * **The tick is a Web Worker's own**, so its timer runs on another thread:
 * a tick that REACHES THE PAGE during a frozen frame loop proves the page
 * was running while frames were not, and a tick the page receives late
 * means the main thread was blocked and the message queued. A page that
 * resumes after a suspension fires its ONE overdue interval first - the
 * record's `worker_beats` read against `worker_expected_beats` is what tells
 * that from a page that kept running.
 *
 * The stamp is this worker's monotonic `performance.now()`; the page times
 * the arrival on its own monotonic clock and takes the minimum difference
 * as the offset between the two, so a wall-clock step cannot read as
 * lateness. Nothing here draws or decides anything.
 */

import { HEARTBEAT_TICK_MS } from './heartbeat';

setInterval(() => {
  postMessage({ t: performance.now() });
}, HEARTBEAT_TICK_MS);
