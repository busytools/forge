// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { AgentRow, HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { closeCommand, closeLanding, closeSeat } from './close';

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

/** The fixture's home with exactly the rows named. */
function home(...slots: SessionSlot[]): HomeWire {
  return { ...homeWire, agents: slots.map(agent) };
}

const NOW = 0;

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

  it('lands a lead close on the next row the rail draws', () => {
    expect(closeLanding(home(LEAD, W1, W2), LEAD, NOW)).toEqual({ name: 'session', slot: W1 });
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

describe('closing a seat', () => {
  const command = { close_worker: { project_key: '<fixture>-proj', label: 'w1' } };

  /** A connection whose dispatch is a mock the test can read back. */
  function connection(send: (command: unknown) => unknown = () => null) {
    const dispatch = vi.fn(send);
    return { open: { dispatch } as unknown as Parameters<typeof closeSeat>[0], dispatch };
  }

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
