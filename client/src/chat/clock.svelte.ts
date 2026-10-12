import { duration } from './numbers';
import { runningMs } from './report';
import type { TurnInfo } from './units';

/**
 * The clock a running row draws: the tenths keep moving locally, and the
 * server's own span corrects the reading when a frame lands.
 *
 * **The tick is the fix for the stutter** (Ved, live look 2026-10-11: "the
 * duration, it is very stuttering... coming from the socket"). The row's
 * reading is `duration_ms + the wait since the last frame`, which cancels to
 * `now - turn start` - so the number only moved when the last tick fired, and
 * the tenths it shows (`12.3s`) stepped in whole seconds with whatever phase
 * and jitter a loaded main thread left the timer on. His rule for it: expect
 * locally, correct when the detail lands. The tick is fast enough that the
 * tenths move rather than step, and the reading only corrects FORWARD - a
 * frame whose measured span sits behind the local reading (a delivery lag, a
 * fold window that starts later) may never walk the display back, which is
 * the jump the eye catches.
 *
 * **Reading `running` as a value keeps the interval from re-arming per
 * frame**: the record is a fresh object each frame, and an effect reading it
 * restarts the interval before it can fire - the 2026-10-09 freeze, "it
 * stops and then it just jumps to a bigger value".
 *
 * `active` is the extra gate a body draws with: a closed report holds no
 * clock, and a tick recomputing for one would be work nobody sees.
 */
export function runningClock(
  info: () => TurnInfo,
  active: () => boolean = () => true,
): () => string {
  let now = $state(Date.now());
  const running = $derived(info().running);
  $effect(() => {
    if (!running || !active()) return;
    const id = setInterval(() => {
      now = Date.now();
    }, 100);
    return () => clearInterval(id);
  });
  /**
   * The highest the clock has read this turn.
   *
   * Reset the moment the turn stops running, so the settled row draws the
   * true span and the next turn starts a clock of its own. The display takes
   * `max(floor, reading)` rather than the floor alone: the server renderer
   * runs no effects, and a row rendered without the floor must still read its
   * record - the floor is the clamp over the reading, never its source.
   */
  let floor = $state(0);
  $effect(() => {
    const held = info();
    if (!held.running) {
      floor = 0;
      return;
    }
    const reading = runningMs(held, now);
    if (reading > floor) floor = reading;
  });
  return () =>
    info().running
      ? duration(Math.max(floor, runningMs(info(), now)))
      : duration(info().duration_ms);
}
