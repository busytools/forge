// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * The link the surface's store reaches, held here so the mount draws without
 * a socket: nothing in this file is about the wire.
 */
const link = vi.hoisted(() => ({
  attachable: true,
  show: () => Promise.resolve(1),
  hide: () => Promise.resolve(),
  input: () => {},
  resize: () => {},
  pause: () => {},
  control: () => {},
  onFrame: () => () => {},
  onView: () => () => {},
  onControl: () => () => {},
  onState: () => () => {},
}));

vi.mock('./link', () => ({
  clearTerminalLink: vi.fn(),
  installTerminal: vi.fn(),
  terminalLink: () => link,
}));

import Terminal from './Terminal.svelte';
import { handOff, surface } from './surface.svelte';
import { terminalRecord } from './record.svelte';

const seat = { org: 'TestOrg', project: 'proj', label: 'lead' };

let shown: ReturnType<typeof mount> | null = null;

/** Mount the raised surface with the store already live. */
function open(): void {
  surface.phase = 'live';
  surface.seat = seat;
  surface.id = 1;
  surface.viewer = null;
  surface.controller = null;
  surface.control_pending = null;
  surface.control_reason = null;
  terminalRecord.seat = seat;
  terminalRecord.terminal = null;
  const target = document.createElement('div');
  document.body.appendChild(target);
  shown = mount(Terminal, { target });
  flushSync();
}

/** The bar's take/release door, which the surface draws while it is live. */
function controlDoor(): HTMLButtonElement | null {
  const found = document.querySelector('.tn-control');
  return found instanceof HTMLButtonElement ? found : null;
}

beforeEach(() => {
  // xterm's DPR service registers through the legacy listener pair, so a
  // matchMedia stub needs both spellings.
  vi.stubGlobal('matchMedia', () => ({
    matches: false,
    media: '',
    onchange: null,
    addListener: () => {},
    removeListener: () => {},
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  }));
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe(): void {}
      disconnect(): void {}
    },
  );
});

afterEach(() => {
  // **The store outlives a mount**, so the case's own surface is put back
  // before the next one mounts it.
  surface.phase = 'closed';
  surface.seat = null;
  surface.id = null;
  surface.control_reason = null;
  terminalRecord.seat = null;
  terminalRecord.terminal = null;
  handOff.current = null;
  if (shown !== null) void unmount(shown);
  shown = null;
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
});

describe('the raised terminal bar', () => {
  /** **A parked hand-off does not close the take-control door.** The ask
   *  raises the surface as a viewer rather than handing out the keyboard, so
   *  this press is how the person types; the book's bar row says the door is
   *  drawn whenever the surface is live, a parked hand-off included. */
  it('draws take control while a terminal hand-off is parked', () => {
    handOff.current = {
      id: 'h1',
      reason: 'sudo needs the account password before the reinstall can run',
      secret: false,
      respond: () => {},
    };
    open();

    const door = controlDoor();
    expect(door, 'the bar drew no take-control door').not.toBeNull();
    expect(door?.textContent?.trim()).toBe('take control');
    // The parked ask is on the bar at the same time, so the door is drawn
    // beside a real hand-off rather than on a bar with no ask at all.
    expect(document.body.textContent, 'the ask is not the one drawn').toContain(
      'sudo needs the account password',
    );
    expect(document.body.textContent, 'the ask has no answer door').toContain('Done');
  });
});
