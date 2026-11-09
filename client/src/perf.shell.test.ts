// @vitest-environment jsdom
/**
 * The frame probe under a shell: the resume the DOM does not report.
 *
 * **A mobile resume is not a DOM event here.** Android reports neither focus
 * nor reliably a visibility change (`connect/resync.ts` listens for the
 * shell's `client://foreground` for the same reason), so without that event
 * the probe would stay stopped after a lock with nothing to re-arm it.
 */
import { describe, expect, it, vi } from 'vitest';

const { foreground } = vi.hoisted(() => ({
  foreground: [] as ((event: unknown) => void)[],
}));

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
  isTauri: () => true,
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, cb: (event: unknown) => void) => {
    if (name === 'client://foreground') foreground.push(cb);
    return Promise.resolve(() => {
      foreground.length = 0;
    });
  },
}));

import { perfProbe } from './perf';

describe('the frame probe under a shell', () => {
  it('arms from the shell resume after a hidden period', async () => {
    // `canHost()` reads the shell's marker on the window, which jsdom lacks.
    const globals = window as unknown as Record<string, unknown>;
    globals['__TAURI_INTERNALS__'] = {};
    const focus = vi.spyOn(document, 'hasFocus').mockReturnValue(true);
    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(false);
    const callbacks: { run: () => void; live: boolean }[] = [];
    let cancelled = false;
    const probe = perfProbe({
      now: () => 0,
      schedule: (cb) => {
        const entry = { run: cb, live: true };
        callbacks.push(entry);
        return () => {
          entry.live = false;
          cancelled = true;
        };
      },
      sink: () => {},
    });
    const scheduled = (): boolean => callbacks.some((entry) => entry.live);
    try {
      probe.start();
      await vi.waitFor(() =>
        expect(foreground, 'the shell resume listener is installed').toHaveLength(1),
      );

      hidden.mockReturnValue(true);
      document.dispatchEvent(new Event('visibilitychange'));
      expect(scheduled(), 'a hidden page schedules nothing').toBe(false);

      // Android's resume: no focus, no visibility change. The shell's own
      // event is the only word the page gets.
      foreground[0]?.({});
      expect(scheduled(), 'the shell resume arms the loop').toBe(true);
    } finally {
      probe.stop();
      delete globals['__TAURI_INTERNALS__'];
      hidden.mockRestore();
      focus.mockRestore();
    }
    expect(cancelled, 'stop cancels the frame').toBe(true);
  });
});
