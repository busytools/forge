/**
 * Closing a seat from the rail, and where the reader lands after it.
 *
 * The terminal's own two gestures: a worker's row closes that worker
 * through `close_worker`, a project's row closes its lead through
 * `close_session` (which the core cascades to the project's live workers).
 * The worker's command is not interchangeable with the lead's - on a
 * worker's slot the lead's command only releases the session and leaves
 * the row reading Running - so which one a row sends is the row's own.
 *
 * And the terminal's other half: a seat REMOVED under the reader - by a
 * cascade, by another view's despawn - is not somewhere to leave them, so
 * [`watchRemovals`] lands them the way its `WorkerStatusChanged` handler
 * does.
 */

import type { Command, ServerMessage } from '../protocol';
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

/**
 * The seats this client has closed, until the roster catches up.
 *
 * The terminal knows a closed seat is gone because it dropped the bucket
 * itself, in the same breath as the click; a client's roster is a read
 * that lands later, so for a moment a closed lead still reads as live and
 * the cascade's removals would land the reader back on it.
 */
const closedHere = new Set<string>();

/**
 * Forget the closed seats the roster has caught up with, so the set stays
 * small and a project started again later is not suppressed by an old mark.
 */
export function forgetClosed(home: HomeWire): void {
  for (const key of closedHere) {
    if (!home.agents.some((agent) => keyOf(agent.slot) === key)) closedHere.delete(key);
  }
}

/**
 * Whether a seat has a session behind it: named by the roster, awake, and
 * not one this client has just closed.
 *
 * A landing on a seat with nothing behind it is a failure twice over: the
 * page for such a seat draws a refusal, or - for a lead the roster no
 * longer names - reads as "start this project" and spawns it, so what
 * looked like a close would have started something instead.
 */
function behind(home: HomeWire, slot: SessionSlot): boolean {
  const key = keyOf(slot);
  if (closedHere.has(key)) return false;
  return home.agents.some(
    (agent) =>
      keyOf(agent.slot) === key &&
      agent.lifecycle !== 'Sleeping' &&
      agent.lifecycle !== 'LoggedOut',
  );
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

/** Every seat the rail draws that has a session behind it, in its drawn order. */
function liveSlots(home: HomeWire, slot: SessionSlot, now: number): SessionSlot[] {
  // The seat the rail marks affects nothing here - the groups, their order
  // and each project's rows come from the states and not from the mark.
  return railGroups(home, slot, now)
    .flatMap((group) =>
      group.projects.flatMap((project) => [
        project.row.slot,
        ...project.workers.map((row) => row.slot),
        ...project.sleeping.map((row) => row.slot),
      ]),
    )
    .filter((candidate) => behind(home, candidate));
}

/**
 * The next live row after `closed`: forward to the end, then the rows
 * before it in reverse - the walk the terminal makes - and the home when
 * no row is left.
 */
function walk(home: HomeWire, closed: SessionSlot, now: number): Route {
  const key = keyOf(closed);
  const drawn = liveSlots(home, closed, now);
  const at = drawn.findIndex((candidate) => keyOf(candidate) === key);
  const order = at === -1 ? drawn : [...drawn.slice(at + 1), ...drawn.slice(0, at).reverse()];
  const next = order.find((candidate) => keyOf(candidate) !== key);
  return next === undefined ? { name: 'home' } : { name: 'session', slot: next };
}

/**
 * Where the reader lands after closing `slot`.
 *
 * A worker's close lands on its lead, the seat that owns it. One with no
 * live lead, and every lead close, lands on the walk's next live row, so a
 * close never hands the reader a seat with nothing behind it.
 */
export function closeLanding(home: HomeWire, slot: SessionSlot, now: number): Route {
  const lead: SessionSlot = { org: slot.org, project: slot.project, label: 'lead' };
  if (slot.label !== 'lead' && behind(home, lead)) {
    return { name: 'session', slot: lead };
  }
  return walk(home, slot, now);
}

/**
 * Where the reader lands when `slot` is removed under them: its spawning
 * lead when that still has a session behind it, else the walk's next live
 * row, else the home.
 *
 * The lead is the terminal's own first answer for a removed worker, and
 * the walk is why it runs second - a cascade removes a lead and its
 * workers together, so the lead is often the seat that just went.
 */
export function removedLanding(
  home: HomeWire,
  slot: SessionSlot,
  spawnedBy: SessionSlot | null,
  now: number,
): Route {
  const lead = spawnedBy ?? { org: slot.org, project: slot.project, label: 'lead' };
  if (behind(home, lead)) return { name: 'session', slot: lead };
  return walk(home, slot, now);
}

/** The seat a `worker_status_changed` update removed, and the lead that spawned it. */
export function removedSeat(
  message: ServerMessage,
): { seat: SessionSlot; spawnedBy: SessionSlot | null } | null {
  if (message.kind !== 'update') return null;
  const update = message.update;
  if (typeof update === 'string') return null;
  const payload = update['worker_status_changed'];
  if (payload === null || typeof payload !== 'object') return null;
  const held = payload as { action?: unknown; status?: unknown };
  if (held.action !== 'removed') return null;
  const fields = (held.status ?? null) as Record<string, unknown> | null;
  if (fields === null) return null;
  const seat = asSlot(fields['slot']);
  if (seat === null) return null;
  return { seat, spawnedBy: asSlot(fields['spawned_by']) };
}

/** A field that is a session slot, or `null` for anything else. */
function asSlot(value: unknown): SessionSlot | null {
  if (value === null || typeof value !== 'object') return null;
  const fields = value as Record<string, unknown>;
  if (
    typeof fields['org'] !== 'string' ||
    typeof fields['project'] !== 'string' ||
    typeof fields['label'] !== 'string'
  ) {
    return null;
  }
  return { org: fields['org'], project: fields['project'], label: fields['label'] };
}

/**
 * Watch for seats the core removes under the reader.
 *
 * A close made here already lands the reader (`closeSeat`); this is the
 * other door - a lead's cascade releasing the workers under it, a despawn
 * from another view, anything that takes the seat the page is showing with
 * no click here. The caller decides what to do with each removal; the
 * terminal's own answer is [`removedLanding`].
 */
export function watchRemovals(
  connection: Connection,
  onRemoved: (seat: SessionSlot, spawnedBy: SessionSlot | null) => void,
): () => void {
  return connection.onMessage((message) => {
    const removed = removedSeat(message);
    if (removed !== null) onRemoved(removed.seat, removed.spawnedBy);
  });
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
  // Marked before the landing: the walk must not offer the seat that is
  // being closed, whatever the roster still says about it.
  closedHere.add(keyOf(slot));
  if (keyOf(current) === keyOf(slot)) {
    goTo(closeLanding(home, slot, now));
  }
  return true;
}
