/**
 * The page's own silence: a link that says it is open while nothing arrives.
 *
 * **The transports' probes answer a healthy link, and their own deadlines
 * answer a dead one.** A browser page asks after 15 s of quiet and reaps at
 * 65 s; the desktop shell judges its beat in bytes and reaps sooner. Both turn
 * a quiet link into a status the shell already draws as "reconnecting".
 *
 * What neither can catch is a quiet link whose own machinery has stopped
 * reporting. On 2026-11-08 the server process stayed alive with its runtime
 * wedged: no FIN, no RST, no error, nothing for two hours, and the page kept
 * painting its last read as live. The page's clock is the backstop - it runs
 * where the wedged half does not - and it is the only evidence a reader gets
 * that the server has stopped answering.
 */

import { LIVENESS_PROTOCOL, type ConnectionStatus } from './socket';

/**
 * How long an open link may be quiet before the page calls it silent.
 *
 * **Past the transports' own detect window**, which is a deliberate 80 s in a
 * browser: a slow write is not silence, and the server bounds a single write
 * at 60 s (`crates/forge-server/src/transport.rs`). A page that drew this
 * line inside that window would cry wolf on every large seat delivery. Past
 * it, a healthy link has either answered a probe or been closed by its
 * transport, so the line only ever appears when something has genuinely
 * stopped reporting.
 */
export const SILENCE_MS = 90_000;

/**
 * The silence a page should draw, in milliseconds, or `null` when the link is
 * current.
 *
 * - `status` is the link's own: a `connecting` or `closed` link already has
 *   its reconnect line, and this is not a second sentence about it.
 * - `protocol` gates the rule to a server that can answer the probe. A
 *   pre-liveness server's quiet is not evidence of anything, so it is never
 *   reported as a fault. `null` is a server that has not greeted: a link that
 *   has carried no frame at all for the bound is dead whatever it would have
 *   spoken.
 * - `heardAt` is when a frame last arrived, stamped by the connection at each
 *   open and on every inbound frame.
 */
export function silenceOf(input: {
  status: ConnectionStatus;
  protocol: number | null;
  heardAt: number | null;
  now: number;
}): number | null {
  if (input.status !== 'open') return null;
  if (input.protocol !== null && input.protocol < LIVENESS_PROTOCOL) return null;
  if (input.heardAt === null) return null;
  const quiet = input.now - input.heardAt;
  return quiet >= SILENCE_MS ? quiet : null;
}
