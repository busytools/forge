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
  onOutput,
  openTerminal,
  surface,
} from './surface.svelte';
import type { Connection } from '../socket';

const seat = { org: 'TestOrg', project: 'proj', label: 'lead' };

describe('the terminal surface store', () => {
  afterEach(async () => {
    await closeTerminal();
  });

  it('opens, answers a repaint, and writes only the frames after it', async () => {
    const stop = installTerminalSurface({} as Connection);
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
    const stop = installTerminalSurface({} as Connection);
    await openTerminal(seat, 4);
    expect(await closeTerminal()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(link.hide).toHaveBeenCalled();
    expect(link.close).not.toHaveBeenCalled();
    stop();
  });

  it('fails the surface with the server sentence when the attach refuses', async () => {
    const stop = installTerminalSurface({} as Connection);
    link.show.mockRejectedValueOnce(new Error('this seat has no terminal open'));
    expect(await openTerminal(seat, 4)).toBe('this seat has no terminal open');
    expect(surface.phase).toBe('failed');
    stop();
  });
});
