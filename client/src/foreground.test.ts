// @vitest-environment jsdom
/**
 * The shell resume arm on its own: the no-shell case, the live arm, and the
 * disconnect that arrives before the lazy import resolves.
 *
 * Three readers share this arm now, so its own branches are pinned here
 * rather than only through a consumer.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { callbacks, installed } = vi.hoisted(() => ({
  callbacks: [] as ((event: unknown) => void)[],
  installed: [] as ReturnType<typeof vi.fn>[],
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, cb: (event: unknown) => void) => {
    if (name === 'client://foreground') callbacks.push(cb);
    const stop = vi.fn();
    installed.push(stop);
    return Promise.resolve(stop);
  },
}));

import { observeForeground } from './foreground';

/** Pretend this page runs inside the shell, so `canHost` says yes. */
function inShell(on: boolean): void {
  const held = window as unknown as Record<string, unknown>;
  if (on) held['__TAURI_INTERNALS__'] = {};
  else delete held['__TAURI_INTERNALS__'];
}

describe('the shell foreground arm', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    callbacks.length = 0;
    installed.length = 0;
    inShell(false);
  });

  afterEach(() => {
    vi.useRealTimers();
    inShell(false);
  });

  it('is not installed where there is no shell', () => {
    expect(observeForeground('the test arm', () => {})).toBeNull();
    expect(installed, 'a listener was installed with no shell').toHaveLength(0);
  });

  it('delivers the resume and gives the listener back when stopped', async () => {
    inShell(true);
    const seen: number[] = [];
    const stop = observeForeground('the test arm', () => seen.push(1));
    // The event module is a lazy import, so the listener lands a microtask
    // after the arm is made.
    await vi.advanceTimersByTimeAsync(0);
    expect(installed, 'the listener was not installed under a shell').toHaveLength(1);

    callbacks[0]?.({});
    expect(seen, 'the resume was not delivered').toHaveLength(1);

    stop?.();
    expect(installed[0], 'the listener was not given back').toHaveBeenCalled();
  });

  it('registers nothing when stopped before the import resolves', async () => {
    inShell(true);
    const seen: number[] = [];
    const stop = observeForeground('the test arm', () => seen.push(1));
    stop?.();
    await vi.advanceTimersByTimeAsync(0);

    expect(installed, 'a listener was registered for a stopped arm').toHaveLength(0);
    callbacks[0]?.({});
    expect(seen, 'a stopped arm delivered a resume').toHaveLength(0);
  });
});
