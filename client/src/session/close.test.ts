// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { ServerMessage } from '../protocol';
import type { AgentRow, HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import {
  closeCommand,
  closeLanding,
  closeSeat,
  forgetClosed,
  removedLanding,
  removedSeat,
  watchRemovals,
} from './close';

/**
 * The close chip's two questions, apart from the markup that asks them: what
 * a row's close sends, and where the reader lands afterwards. Both are
 * rail rules, so they are asserted against the rail's own order.
 */

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const W1: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'w1' };
const W2: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'w2' };

/** One row, in the fixture's own shape. */
function agent(slot: SessionSlot): AgentRow {
  const template = homeWire.agents[0];
  if (template === undefined) throw new Error('the fixture holds no agent');
  return { ...template, slot, label: slot.label, lifecycle: 'Running' };
}

/** One row in the state named. */
function agentIn(slot: SessionSlot, lifecycle: AgentRow['lifecycle']): AgentRow {
  return { ...agent(slot), lifecycle };
}

/** The fixture's home with exactly the rows named. */
function home(...slots: SessionSlot[]): HomeWire {
  return { ...homeWire, agents: slots.map(agent) };
}

/**
 * The fixture's home with these projects declared and these rows on them.
 *
 * More than one project is what tells the landing's lead preference apart
 * from its walk: with a single project both answer the same row.
 */
function ground(projects: string[], ...slots: SessionSlot[]): HomeWire {
  const template = homeWire.projects[0];
  if (template === undefined) throw new Error('the fixture holds no project');
  return {
    ...homeWire,
    projects: projects.map((name) => ({
      ...template,
      project: { ...template.project, name, key: `key-${name}` },
    })),
    agents: slots.map(agent),
  };
}

const NOW = 0;

beforeEach(() => {
  // The closed-seat marks are module state; an empty roster forgets all of
  // them, which the production function does on its own terms.
  forgetClosed({ ...homeWire, agents: [] });
});

describe("what a row's close sends", () => {
  it('closes a worker with close_worker, under the project key the rail holds', () => {
    expect(closeCommand(home(LEAD, W1), W1)).toEqual({
      close_worker: { project_key: '<fixture>-proj', label: 'w1' },
    });
  });

  /**
   * The lead's command on a worker slot only releases the session and leaves
   * the worker's row reading Running, which is why a worker's row may not
   * send it.
   */
  it('closes a lead with close_session, on the slot itself', () => {
    expect(closeCommand(home(LEAD, W1), LEAD)).toEqual({ close_session: { session_key: LEAD } });
  });

  it('has no command for a seat whose project this roster does not name', () => {
    const elsewhere: SessionSlot = { org: 'TestOrg', project: 'gone', label: 'w1' };
    expect(closeCommand(home(LEAD), elsewhere)).toBeNull();
  });
});

describe('where a close lands the reader', () => {
  it("lands a worker's close on its lead, the seat that owns it", () => {
    expect(closeLanding(home(LEAD, W1), W1, NOW)).toEqual({ name: 'session', slot: LEAD });
  });

  /**
   * **A lead's close takes its whole project with it** (#1703): the core
   * cascades `close_session` to the project's live workers, so every seat
   * this roster still names there is closing - offering one landed the
   * reader on a session-less seat and drew the refusal before anything else
   * happened (Ved's live find). The landing reads the rail's own order from
   * the top instead: the first live project outside the closing one, not
   * the walk's adjacent pick.
   */
  it("lands a lead's close on the first live project, outside its own", () => {
    const held = ground(['proj', 'other'], LEAD, W1, OTHER_LEAD, OTHER_W1);
    expect(closeLanding(held, LEAD, NOW)).toEqual({ name: 'session', slot: OTHER_LEAD });
  });

  it('reads the rail from the top, not the walk from the closed row', () => {
    // The closed lead sits LAST; the walk from it wraps onto the second
    // project, and the landing must be the FIRST live project instead.
    const a: SessionSlot = { org: 'TestOrg', project: 'a', label: 'lead' };
    const b: SessionSlot = { org: 'TestOrg', project: 'b', label: 'lead' };
    const held = ground(['a', 'b', 'proj'], a, b, LEAD, W1);
    expect(closeLanding(held, LEAD, NOW)).toEqual({ name: 'session', slot: a });
  });

  /**
   * The walk is the terminal's: forward to the last row, then the rows
   * before it in reverse - so closing the last row lands on the one above
   * it rather than falling past everything.
   */
  it('wraps: closing the last row lands on the row above it', () => {
    expect(closeLanding(home(W1, W2), W2, NOW)).toEqual({ name: 'session', slot: W1 });
  });

  it('lands on the home when no row is left behind it', () => {
    expect(closeLanding(home(LEAD), LEAD, NOW)).toEqual({ name: 'home' });
  });

  it('lands a worker with no lead row on the next drawn row', () => {
    expect(closeLanding(home(W1, W2), W1, NOW)).toEqual({ name: 'session', slot: W2 });
  });
});

/** A connection whose dispatch is a mock the test can read back, and whose messages it can feed. */
function connection(send: (command: unknown) => unknown = () => null) {
  const dispatch = vi.fn(send);
  const listeners = new Set<(message: ServerMessage) => void>();
  const open = {
    dispatch,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
  } as unknown as Parameters<typeof closeSeat>[0];
  return {
    open,
    dispatch,
    deliver: (message: ServerMessage): void => {
      for (const listener of listeners) listener(message);
    },
  };
}

describe('closing a seat', () => {
  const command = { close_worker: { project_key: '<fixture>-proj', label: 'w1' } };

  it("sends the row's command and moves off the seat the reader was on", () => {
    history.replaceState(null, '', '/session/TestOrg/proj/w1');
    const { open, dispatch } = connection();
    expect(closeSeat(open, home(LEAD, W1), W1, W1, NOW)).toBe(true);
    expect(dispatch).toHaveBeenCalledWith(command);
    expect(location.pathname, 'the reader was left on the seat that closed').toBe(
      '/session/TestOrg/proj/lead',
    );
  });

  it('leaves the reader where they are when another row was closed', () => {
    history.replaceState(null, '', '/session/TestOrg/proj/w2');
    const { open, dispatch } = connection();
    expect(closeSeat(open, home(LEAD, W1, W2), W1, W2, NOW)).toBe(true);
    expect(dispatch).toHaveBeenCalledWith(command);
    expect(location.pathname, 'a close elsewhere moved the reader').toBe(
      '/session/TestOrg/proj/w2',
    );
  });

  /**
   * A socket that is closed throws rather than answering, and nothing went:
   * navigating would leave the reader on another seat with the seat they
   * asked to close still running.
   */
  it('does nothing at all when the command did not go', () => {
    history.replaceState(null, '', '/session/TestOrg/proj/w1');
    const { open } = connection(() => {
      throw new Error('the socket is closed');
    });
    expect(closeSeat(open, home(LEAD, W1), W1, W1, NOW)).toBe(false);
    expect(location.pathname, 'a close that never went moved the reader').toBe(
      '/session/TestOrg/proj/w1',
    );
  });
});

const OTHER_LEAD: SessionSlot = { org: 'TestOrg', project: 'other', label: 'lead' };
const OTHER_W1: SessionSlot = { org: 'TestOrg', project: 'other', label: 'w1' };
const OTHER_W2: SessionSlot = { org: 'TestOrg', project: 'other', label: 'w2' };

describe('the walk is over seats with something behind them', () => {
  /**
   * A sleeping seat is a row the rail draws with nothing running behind it,
   * and landing on one draws a refusal - or, for a lead the roster no
   * longer names, a page that starts the project. The terminal's own
   * candidate walk can never hold such a row, so neither may this one.
   */
  it('skips a sleeping row rather than hand the reader a seat with no session', () => {
    const held: HomeWire = { ...homeWire, agents: [agent(LEAD), agentIn(W1, 'Sleeping')] };
    expect(closeLanding(held, LEAD, NOW)).toEqual({ name: 'home' });
  });

  /**
   * The lead preference and the walk answer differently once a second
   * project is on the rail: without the preference, closing this worker
   * walks onto the NEXT project's lead.
   */
  it("prefers the closed worker's own lead, not the walk's next row", () => {
    const held = ground(['proj', 'other'], LEAD, W1, OTHER_LEAD);
    expect(closeLanding(held, W1, NOW)).toEqual({ name: 'session', slot: LEAD });
  });

  /**
   * The direction, which only a middle row can tell: closing the second
   * project's worker - a project with no live lead, so the walk answers -
   * has a live row after it and rows before it, and the walk goes FORWARD
   * first. (A lead close would land on the first live project, which is the
   * other describe's rule, not the walk's.)
   */
  it('walks forward from a middle row rather than back', () => {
    const held = ground(['proj', 'other'], W1, OTHER_W1, OTHER_W2);
    expect(closeLanding(held, OTHER_W1, NOW)).toEqual({ name: 'session', slot: OTHER_W2 });
  });
});

/** The update a removed worker arrives as. */
function removed(seat: SessionSlot, by: SessionSlot | null, action = 'removed'): ServerMessage {
  return {
    kind: 'update',
    update: { worker_status_changed: { action, status: { slot: seat, spawned_by: by } } },
  };
}

describe('a seat removed under the reader', () => {
  it('reads the removed seat and the lead that spawned it from the update', () => {
    expect(removedSeat(removed(W1, LEAD))).toEqual({ seat: W1, spawnedBy: LEAD });
    expect(
      removedSeat(removed(W1, LEAD, 'status_changed')),
      'a status change is not a removal',
    ).toBeNull();
    expect(removedSeat({ kind: 'update', update: { chat_appended: {} } })).toBeNull();
  });

  /**
   * The worker is not the last row and a second project is behind it, so the
   * walk and the lead preference answer differently: the hop is what the
   * name claims, not the wrap.
   */
  it('lands a removed worker on its spawning lead', () => {
    const held = ground(['proj', 'other'], LEAD, W1, OTHER_LEAD);
    expect(removedLanding(held, W1, LEAD, NOW)).toEqual({ name: 'session', slot: LEAD });
  });

  /**
   * The cascade: a lead close removes the workers under it, and the lead
   * itself is gone - so the redirect must fall past the seat that just
   * went rather than back onto it.
   */
  it('falls past a lead this client has just closed', () => {
    const held = ground(['proj', 'other'], LEAD, W1, OTHER_LEAD);
    const { open } = connection();
    history.replaceState(null, '', '/session/TestOrg/proj/lead');
    expect(closeSeat(open, held, LEAD, LEAD, NOW)).toBe(true);
    expect(removedLanding(held, W1, LEAD, NOW)).toEqual({ name: 'session', slot: OTHER_LEAD });
  });

  /**
   * The mark lasts only until the roster catches up. A project started
   * again later is not suppressed by a close from before, which is what
   * forgetting a seat the roster no longer names buys.
   */
  it('forgets a closed seat once the roster stops naming it', () => {
    const first = ground(['proj', 'other'], LEAD, W1, OTHER_LEAD);
    const caughtUp = ground(['proj', 'other'], W1, OTHER_LEAD);
    const restarted = ground(['proj', 'other'], LEAD, W1, OTHER_LEAD);
    const { open } = connection();
    history.replaceState(null, '', '/session/TestOrg/proj/lead');
    expect(closeSeat(open, first, LEAD, LEAD, NOW)).toBe(true);
    expect(removedLanding(first, W1, LEAD, NOW), 'the mark was not in force').toEqual({
      name: 'session',
      slot: OTHER_LEAD,
    });

    forgetClosed(caughtUp);
    expect(
      removedLanding(restarted, W1, LEAD, NOW),
      'a project started again stayed suppressed',
    ).toEqual({ name: 'session', slot: LEAD });
  });

  it('hands each removal to the watcher, and nothing else', () => {
    const seen: Array<[SessionSlot, SessionSlot | null]> = [];
    const { open, deliver } = connection();
    const stop = watchRemovals(open, (seat, by) => seen.push([seat, by]));
    deliver(removed(W1, LEAD));
    deliver({ kind: 'update', update: { chat_appended: {} } });
    stop();
    deliver(removed(W2, LEAD));
    expect(seen).toEqual([[W1, LEAD]]);
  });
});
