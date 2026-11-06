// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';

/**
 * The link the store drives, held here so the test can push a repaint and a
 * frame through the same listeners the real link would call.
 */
type Listener<T> = (value: T) => void;
const pushed: {
  view: Listener<{
    seat: { org: string; project: string; label: string };
    id: number;
    cols: number;
    rows: number;
    seq: number;
    screen: string;
    viewers: number;
    viewer?: number;
    controller?: number | null;
  }> | null;
  frame: Listener<{ id: number; seq: number; bytes: Uint8Array }> | null;
  control: Listener<{
    seat: { org: string; project: string; label: string };
    id: number;
    controller: number | null;
  }> | null;
  state: Listener<{ seat: unknown; terminal: unknown }> | null;
} = { view: null, frame: null, control: null, state: null };

const link = {
  attachable: true,
  show: vi.fn((_seat: unknown, id: number) => Promise.resolve(id)),
  hide: vi.fn(() => Promise.resolve()),
  input: vi.fn(),
  resize: vi.fn(),
  pause: vi.fn(),
  control: vi.fn(),
  onFrame: (fn: Listener<{ id: number; seq: number; bytes: Uint8Array }>) => {
    pushed.frame = fn;
    return () => {
      pushed.frame = null;
    };
  },
  onView: (
    fn: Listener<{
      seat: { org: string; project: string; label: string };
      id: number;
      cols: number;
      rows: number;
      seq: number;
      screen: string;
      viewers: number;
    }>,
  ) => {
    pushed.view = fn;
    return () => {
      pushed.view = null;
    };
  },
  onControl: (
    fn: Listener<{
      seat: { org: string; project: string; label: string };
      id: number;
      controller: number | null;
    }>,
  ) => {
    pushed.control = fn;
    return () => {
      pushed.control = null;
    };
  },
  onState: (fn: Listener<{ seat: unknown; terminal: unknown }>) => {
    pushed.state = fn;
    return () => {
      pushed.state = null;
    };
  },
};

vi.mock('./link', () => ({
  clearTerminalLink: vi.fn(),
  installTerminal: vi.fn(),
  terminalLink: () => link,
}));

import {
  closeTerminal,
  controlTerminal,
  controlling,
  installTerminalSurface,
  attachSurface,
  lowerForOtherSeat,
  mayType,
  onOutput,
  terminalStateLine,
  onScreen,
  openTerminal,
  reconcileSurfaceSize,
  surface,
} from './surface.svelte';
import type { Connection } from '../socket';
import type { TerminalSummary } from '../wire/terminal';

const seat = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** One terminal summary, as the record carries it. */
function terminalSummary(over: Partial<TerminalSummary> = {}): TerminalSummary {
  return {
    id: 1,
    shell: '/bin/zsh',
    cwd: '/tmp',
    cols: 80,
    rows: 24,
    alive: true,
    state: { running: false, input_expected: null, exit: null, source: 'exact' },
    title: '',
    held: false,
    ...over,
  };
}

/** The messages the store listens for, so a test can push one. */
const messageListeners = new Set<
  (message: { kind: string; what?: string; why?: string }) => void
>();

/** A connection whose status never moves and whose messages a test pushes. */
function stillConnection(): Connection {
  return {
    onStatus: () => () => {},
    onMessage: (fn: (message: { kind: string; what?: string; why?: string }) => void) => {
      messageListeners.add(fn);
      return () => messageListeners.delete(fn);
    },
  } as unknown as Connection;
}

function pushError(what: string, why: string): void {
  for (const fn of messageListeners) fn({ kind: 'error', what, why });
}

/**
 * Open the surface and attach it, as the row and the mounted surface do: the
 * attach waits for the grid, so a test that wants the open's answer drives
 * both.
 */
async function opened(id: number | null = 4, cols = 80, rows = 24): Promise<string | null> {
  const opening = openTerminal(seat, id);
  await attachSurface(cols, rows);
  return await opening;
}

describe('the terminal surface store', () => {
  afterEach(async () => {
    await closeTerminal();
  });

  it('opens, answers a repaint, and writes only the frames after it', async () => {
    const stop = installTerminalSurface(stillConnection());
    const written: number[] = [];
    const off = onOutput((frame) => written.push(frame.seq));

    expect(await opened(4)).toBeNull();
    expect(surface.phase).toBe('live');
    expect(surface.id).toBe(4);

    // The attach's repaint reflects every chunk at or below seq 10.
    pushed.view?.({ seat, id: 4, cols: 80, rows: 24, seq: 10, screen: '$ ', viewers: 1 });
    // A chunk the repaint already carries is not written again; the next one
    // is.
    pushed.frame?.({ id: 4, seq: 10, bytes: new Uint8Array([1]) });
    pushed.frame?.({ id: 4, seq: 11, bytes: new Uint8Array([2]) });
    // Another terminal's frame is not this surface's.
    pushed.frame?.({ id: 9, seq: 12, bytes: new Uint8Array([3]) });

    expect(written).toEqual([11]);
    off();
    stop();
  });

  it('lowers the surface without closing the terminal', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    expect(await closeTerminal()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(link.hide).toHaveBeenCalled();
    stop();
  });

  it('fails the surface with the server sentence when the attach refuses', async () => {
    const stop = installTerminalSurface(stillConnection());
    link.show.mockRejectedValueOnce(new Error('this seat has no terminal open'));
    expect(await opened(4)).toBe('this seat has no terminal open');
    expect(surface.phase).toBe('failed');
    stop();
  });

  it('starts the seat a terminal when the attach names none', async () => {
    const stop = installTerminalSurface(stillConnection());
    link.show.mockResolvedValueOnce(7);
    expect(await opened(null)).toBeNull();
    expect(link.show).toHaveBeenCalledWith(seat, null, 80, 24);
    expect(surface.id).toBe(7);
    stop();
  });

  it('keeps a frame that lands before the surface mounts, then writes it', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.view?.({ seat, id: 4, cols: 80, rows: 24, seq: 10, screen: '$ ', viewers: 1 });
    // xterm.js is a lazy chunk: the frames before it loads have no listener
    // to draw them, and a drop here is a hole in the screen.
    pushed.frame?.({ id: 4, seq: 11, bytes: new Uint8Array([2]) });
    const written: number[] = [];
    const off = onOutput((frame) => written.push(frame.seq));
    expect(written).toEqual([11]);
    off();
    stop();
  });

  it('does not write a held frame the newest repaint already carries', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.frame?.({ id: 4, seq: 11, bytes: new Uint8Array([2]) });
    // The attach lands after the chunk: its repaint reflects seq 11, so the
    // held copy would draw the same bytes twice.
    pushed.view?.({ seat, id: 4, cols: 80, rows: 24, seq: 11, screen: '$ ', viewers: 1 });
    const written: number[] = [];
    const off = onOutput((frame) => written.push(frame.seq));
    expect(written).toEqual([]);
    off();
    stop();
  });

  it('takes a repaint when the held window overflowed instead of drawing a fragment', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.view?.({ seat, id: 4, cols: 80, rows: 24, seq: 10, screen: '$ ', viewers: 1 });
    // One frame past the hold limit drops the buffer and asks for a repaint.
    pushed.frame?.({ id: 4, seq: 11, bytes: new Uint8Array(1_000_001) });
    // A later frame lands before the mount; the repaint still owns the
    // screen, and drawing the held tail first would be a fragment of it.
    pushed.frame?.({ id: 4, seq: 12, bytes: new Uint8Array([2]) });
    const written: number[] = [];
    link.show.mockClear();
    const off = onOutput((frame) => written.push(frame.seq));
    await vi.waitFor(() => {
      expect(link.show).toHaveBeenCalledWith(seat, 4, 80, 24);
    });
    expect(written).toEqual([]);
    off();
    stop();
  });

  it('takes the terminal id from the attach when the open named none', async () => {
    const stop = installTerminalSurface(stillConnection());
    let answer: (id: number) => void = () => {};
    link.show.mockImplementationOnce(
      () =>
        new Promise<number>((resolve) => {
          answer = resolve;
        }),
    );
    const opening = openTerminal(seat, null);
    const attaching = attachSurface(80, 24);
    // The repaint lands before the attach's answer, as it does when the two
    // messages cross.
    pushed.view?.({ seat, id: 7, cols: 80, rows: 24, seq: 1, screen: '$ ', viewers: 1 });
    const seen: string[] = [];
    const off = onScreen((view) => seen.push(view.screen));
    answer(7);
    await attaching;
    await opening;
    expect(seen, 'a mount after the repaint still draws it').toEqual(['$ ']);
    off();
    stop();
  });

  it('asks for a repaint laid out at the client size', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.view?.({ seat, id: 4, cols: 120, rows: 30, seq: 1, screen: 'wide', viewers: 1 });
    link.show.mockClear();
    link.resize.mockClear();
    expect(reconcileSurfaceSize(80, 24)).toBe(true);
    expect(link.resize).toHaveBeenCalledWith(seat, 4, 80, 24);
    await vi.waitFor(() => {
      expect(link.show).toHaveBeenCalledWith(seat, 4, 80, 24);
    });
    expect(reconcileSurfaceSize(80, 24), "the size is already the client's").toBe(false);
    stop();
  });

  it('lowers the surface when the page moves to another seat', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    await lowerForOtherSeat({ org: 'TestOrg', project: 'proj', label: 'other' });
    expect(surface.phase).toBe('closed');
    expect(link.hide).toHaveBeenCalled();
    stop();
  });

  it('keeps the surface on the seat it belongs to', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    await lowerForOtherSeat({ ...seat });
    expect(surface.phase).toBe('live');
    stop();
  });

  it('re-attaches a live surface when the connection comes back', async () => {
    const statuses: ((status: 'connecting' | 'open' | 'closed') => void)[] = [];
    const connection = {
      onStatus: (fn: (status: 'connecting' | 'open' | 'closed') => void) => {
        statuses.push(fn);
        return () => {};
      },
      onMessage: () => () => {},
    } as unknown as Connection;
    const stop = installTerminalSurface(connection);
    await opened(4);
    link.show.mockClear();
    statuses[0]?.('open');
    await vi.waitFor(() => {
      expect(link.show).toHaveBeenCalledWith(seat, 4, 80, 24);
    });
    expect(surface.phase).toBe('live');
    stop();
  });

  it("takes and gives back the person's control", async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.view?.({
      seat,
      id: 4,
      cols: 80,
      rows: 24,
      seq: 1,
      screen: '$ ',
      viewers: 1,
      viewer: 9,
      controller: null,
    });
    expect(controlling()).toBe(false);

    controlTerminal(true);
    expect(link.control).toHaveBeenCalledWith(seat, 4, true);
    pushed.control?.({ seat, id: 4, controller: 9 });
    expect(controlling(), 'this page holds the role').toBe(true);

    controlTerminal(false);
    expect(link.control).toHaveBeenCalledWith(seat, 4, false);
    pushed.control?.({ seat, id: 4, controller: null });
    expect(controlling(), 'released, and the agent owns it again').toBe(false);
    stop();
  });

  it("gives a hand-off's keyboard to the one client that claimed it", async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.view?.({
      seat,
      id: 4,
      cols: 80,
      rows: 24,
      seq: 1,
      screen: '$ ',
      viewers: 2,
      viewer: 9,
      controller: null,
    });
    expect(mayType(true), 'the first client to type may claim the keyboard').toBe(true);

    // The claim lands: this client typed first and holds it.
    pushed.control?.({ seat, id: 4, controller: 9 });
    expect(mayType(true), 'the claimer keeps typing').toBe(true);

    // Another client typed first: this one falls back to viewing.
    pushed.control?.({ seat, id: 4, controller: 12 });
    expect(mayType(true), 'the loser of the claim falls back to viewing').toBe(false);
    expect(mayType(false), 'and it cannot type outside a hand-off either').toBe(false);
    expect(controlling(), 'nor does it hold the role').toBe(false);
    stop();
  });

  it('names the keyboard holder whichever door gave the role away', () => {
    const line = (over: {
      terminal?: TerminalSummary | null;
      controller?: number | null;
      controlReason?: string | null;
    }) =>
      terminalStateLine({
        phase: 'live',
        reason: null,
        terminal: over.terminal === undefined ? terminalSummary() : over.terminal,
        viewer: 9,
        controller: over.controller ?? null,
        controlReason: over.controlReason ?? null,
      });

    // Another client took the bar's own door: the loser reads that, not
    // "view only" while another client types.
    expect(line({ controller: 12 }), "another client's take").toBe(
      'another client has the keyboard',
    );
    // A take that landed for this client reads its own role.
    expect(line({ controller: 9 }), 'this client holds it').toBe('you have control');
    // Nobody holds it: the agent's run is what the bar says.
    expect(
      line({ terminal: terminalSummary({ state: { ...terminalSummary().state, running: true } }) }),
      "the agent's run",
    ).toBe('the agent is running a command');
    // A parked hand-off nobody claimed waits for whoever takes it.
    expect(line({ terminal: terminalSummary({ held: true }) }), 'the hand-off waits').toBe(
      'waiting for you',
    );
    // **A parked hand-off another client claimed is not "waiting for you"**:
    // the holder wins the line.
    expect(
      line({ terminal: terminalSummary({ held: true }), controller: 12 }),
      'a hand-off held elsewhere',
    ).toBe('another client has the keyboard');
    // A refusal replaces the line, in the server's own words.
    const refused = 'a terminal hand-off holds the keyboard; use the hand-off card';
    expect(line({ controlReason: refused }), 'the refusal').toBe(refused);
    // **Liveness outranks a refusal**: a sentence about a keyboard says
    // nothing about a terminal that is gone or a shell that exited.
    expect(line({ terminal: null, controlReason: refused }), 'a closed terminal').toBe(
      'the terminal closed',
    );
    expect(
      line({ terminal: terminalSummary({ alive: false }), controlReason: refused }),
      'an exited shell',
    ).toBe('the shell exited');
  });

  it('draws the take press until the server answers, and the refusal when it refuses', async () => {
    const stop = installTerminalSurface(stillConnection());
    await opened(4);
    pushed.view?.({
      seat,
      id: 4,
      cols: 80,
      rows: 24,
      seq: 1,
      screen: '$ ',
      viewers: 1,
      viewer: 9,
      controller: null,
    });

    // **A take press is drawn while the server answers.**
    controlTerminal(true);
    expect(link.control).toHaveBeenCalledWith(seat, 4, true);
    expect(surface.control_pending, 'the take press is drawn').toBe(true);

    // **A fresh repaint is the server's truth**: a press the replaced attach
    // was waiting on is not still waiting.
    pushed.view?.({
      seat,
      id: 4,
      cols: 80,
      rows: 24,
      seq: 2,
      screen: '$ ',
      viewers: 1,
      viewer: 9,
      controller: null,
    });
    expect(surface.control_pending, 'a repaint clears the wait').toBeNull();

    // **The release half is drawn too.**
    controlTerminal(false);
    expect(link.control).toHaveBeenCalledWith(seat, 4, false);
    expect(surface.control_pending, 'the release press is drawn').toBe(false);

    // **Another operation's refusal is not this one's.**
    pushError('terminal_resize', 'the terminal is not that size');
    expect(surface.control_pending, 'an unrelated refusal leaves the wait').toBe(false);
    expect(surface.control_reason, 'and draws nothing').toBeNull();

    // The release lands.
    pushed.control?.({ seat, id: 4, controller: null });
    expect(surface.control_pending, 'the push clears it').toBeNull();

    // **A refusal lands in the state line, in the server's own words.**
    const refused = 'a terminal hand-off holds the keyboard; use the hand-off card';
    controlTerminal(true);
    pushError('terminal_control', refused);
    expect(surface.control_pending).toBeNull();
    expect(surface.control_reason, 'the refusal is drawn as the server said it').toBe(refused);

    // **The control push clears the refusal without a second press.**
    pushed.control?.({ seat, id: 4, controller: 9 });
    expect(surface.control_reason, 'the push clears the refusal').toBeNull();
    expect(controlling(), 'and hands the role over').toBe(true);
    stop();
  });
});
