/**
 * Closing a seat from the rail, and where the reader lands after it.
 *
 * The terminal's own two gestures: a worker's row closes that worker
 * through `close_worker`, a project's row closes its lead through
 * `close_session` (which the core cascades to the project's live workers).
 * The worker's command is not interchangeable with the lead's - on a
 * worker's slot the lead's command only releases the session and leaves
 * the row reading Running - so which one a row sends is the row's own.
 */

import type { Command } from '../protocol';
import { subjectKey } from '../protocol';
import { goTo, type Route } from '../routes';
import type { Connection } from '../socket';
import { report } from '../socket';
import type { HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { railGroups } from './view';

/** A slot's own identity, which is how two seats are told apart here. */
function keyOf(slot: SessionSlot): string {
  return subjectKey({ session: slot });
}

/** The command that closes `slot`, or `null` when this roster cannot address it. */
export function closeCommand(home: HomeWire, slot: SessionSlot): Command | null {
  if (slot.label === 'lead') {
    return { close_session: { session_key: slot } };
  }
  const project = home.projects.find(
    (row) => row.project.org === slot.org && row.project.name === slot.project,
  );
  if (project === undefined) return null;
  return { close_worker: { project_key: project.project.key, label: slot.label } };
}

/** Every seat the rail draws, in the order it draws them. */
function drawnSlots(home: HomeWire, slot: SessionSlot, now: number): SessionSlot[] {
  // The seat the rail marks affects nothing here - the groups, their order
  // and each project's rows come from the states and not from the mark.
  return railGroups(home, slot, now).flatMap((group) =>
    group.projects.flatMap((project) => [
      project.row.slot,
      ...project.workers.map((row) => row.slot),
      ...project.sleeping.map((row) => row.slot),
    ]),
  );
}

/**
 * Where the reader lands after closing `slot`.
 *
 * A worker's close lands on its lead, the seat that owns it. One with no
 * lead row, and every lead close, lands on the next row the rail draws,
 * then on the rows before it in reverse - the walk the terminal makes -
 * and on the home when no row is left.
 */
export function closeLanding(home: HomeWire, slot: SessionSlot, now: number): Route {
  const closed = keyOf(slot);
  const lead: SessionSlot = { org: slot.org, project: slot.project, label: 'lead' };
  if (slot.label !== 'lead' && home.agents.some((row) => keyOf(row.slot) === keyOf(lead))) {
    return { name: 'session', slot: lead };
  }
  const drawn = drawnSlots(home, slot, now);
  const at = drawn.findIndex((candidate) => keyOf(candidate) === closed);
  const walk = at === -1 ? drawn : [...drawn.slice(at + 1), ...drawn.slice(0, at).reverse()];
  const next = walk.find((candidate) => keyOf(candidate) !== closed);
  return next === undefined ? { name: 'home' } : { name: 'session', slot: next };
}

/**
 * Close `slot` and, when the reader was on it, move them off it.
 *
 * Moving off is not a nicety: a closed lead leaves the roster, and a lead
 * seat absent from the roster is what the page reads as "start this
 * project" - staying on it would re-spawn the seat seconds after closing
 * it. A close elsewhere leaves the reader where they are, and one that
 * never went moves nobody.
 */
export function closeSeat(
  connection: Connection,
  home: HomeWire,
  slot: SessionSlot,
  current: SessionSlot,
  now: number,
): boolean {
  const command = closeCommand(home, slot);
  if (command === null) return false;
  try {
    void connection.dispatch(command);
  } catch (error) {
    // A closed socket throws rather than answering, and nothing went.
    report('the close was not sent', error);
    return false;
  }
  if (keyOf(current) === keyOf(slot)) {
    goTo(closeLanding(home, slot, now));
  }
  return true;
}
