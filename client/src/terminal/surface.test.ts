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
  }> | null;
  frame: Listener<{ id: number; seq: number; bytes: Uint8Array }> | null;
  state: Listener<{ seat: unknown; terminal: unknown }> | null;
} = { view: null, frame: null, state: null };

const link = {
  attachable: true,
  show: vi.fn((_seat: unknown, id: number) => Promise.resolve(id)),
  hide: vi.fn(() => Promise.resolve()),
  input: vi.fn(),
  resize: vi.fn(),
  pause: vi.fn(),
  close: vi.fn(),
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
  installTerminalSurface,
  lowerForOtherSeat,
  onOutput,
  onScreen,
  openTerminal,
  reconcileSurfaceSize,
  surface,
} from './surface.svelte';
import type { Connection } from '../socket';

const seat = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A connection whose status never moves: the store only listens for one. */
function stillConnection(): Connection {
  return { onStatus: () => () => {} } as unknown as Connection;
}

describe('the terminal surface store', () => {
  afterEach(async () => {
    await closeTerminal();
  });

  it('opens, answers a repaint, and writes only the frames after it', async () => {
    const stop = installTerminalSurface(stillConnection());
    const written: number[] = [];
    const off = onOutput((frame) => written.push(frame.seq));

    expect(await openTerminal(seat, 4)).toBeNull();
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
    await openTerminal(seat, 4);
    expect(await closeTerminal()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(link.hide).toHaveBeenCalled();
    expect(link.close).not.toHaveBeenCalled();
    stop();
  });

  it('fails the surface with the server sentence when the attach refuses', async () => {
    const stop = installTerminalSurface(stillConnection());
    link.show.mockRejectedValueOnce(new Error('this seat has no terminal open'));
    expect(await openTerminal(seat, 4)).toBe('this seat has no terminal open');
    expect(surface.phase).toBe('failed');
    stop();
  });

  it('starts the seat a terminal when the attach names none', async () => {
    const stop = installTerminalSurface(stillConnection());
    link.show.mockResolvedValueOnce(7);
    expect(await openTerminal(seat, null)).toBeNull();
    expect(link.show).toHaveBeenCalledWith(seat, null);
    expect(surface.id).toBe(7);
    stop();
  });

  it('keeps a frame that lands before the surface mounts, then writes it', async () => {
    const stop = installTerminalSurface(stillConnection());
    await openTerminal(seat, 4);
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
    await openTerminal(seat, 4);
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
    // The repaint lands before the attach's answer, as it does when the two
    // messages cross.
    pushed.view?.({ seat, id: 7, cols: 80, rows: 24, seq: 1, screen: '$ ', viewers: 1 });
    const seen: string[] = [];
    const off = onScreen((view) => seen.push(view.screen));
    answer(7);
    await opening;
    expect(seen, 'a mount after the repaint still draws it').toEqual(['$ ']);
    off();
    stop();
  });

  it('asks for a repaint laid out at the client size', async () => {
    const stop = installTerminalSurface(stillConnection());
    await openTerminal(seat, 4);
    pushed.view?.({ seat, id: 4, cols: 120, rows: 30, seq: 1, screen: 'wide', viewers: 1 });
    link.show.mockClear();
    link.resize.mockClear();
    expect(reconcileSurfaceSize(80, 24)).toBe(true);
    expect(link.resize).toHaveBeenCalledWith(seat, 4, 80, 24);
    await vi.waitFor(() => {
      expect(link.show).toHaveBeenCalledWith(seat, 4);
    });
    expect(reconcileSurfaceSize(80, 24), "the size is already the client's").toBe(false);
    stop();
  });

  it('lowers the surface when the page moves to another seat', async () => {
    const stop = installTerminalSurface(stillConnection());
    await openTerminal(seat, 4);
    await lowerForOtherSeat({ org: 'TestOrg', project: 'proj', label: 'other' });
    expect(surface.phase).toBe('closed');
    expect(link.hide).toHaveBeenCalled();
    stop();
  });

  it('keeps the surface on the seat it belongs to', async () => {
    const stop = installTerminalSurface(stillConnection());
    await openTerminal(seat, 4);
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
    } as unknown as Connection;
    const stop = installTerminalSurface(connection);
    await openTerminal(seat, 4);
    link.show.mockClear();
    statuses[0]?.('open');
    await vi.waitFor(() => {
      expect(link.show).toHaveBeenCalledWith(seat, 4);
    });
    expect(surface.phase).toBe('live');
    stop();
  });
});
