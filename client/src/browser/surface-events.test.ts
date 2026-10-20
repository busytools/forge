// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

const { listen } = vi.hoisted(() => ({ listen: vi.fn() }));

vi.mock('@tauri-apps/api/event', () => ({ listen }));
vi.mock('./host', () => ({
  canHost: () => true,
  drawsSurface: () => true,
  showBrowser: () => Promise.resolve(null),
  hideBrowser: () => Promise.resolve({ lowered: true }),
  reportSurfaceRect: () => Promise.resolve(),
}));

/** The events the shell listeners were registered for, in order. */
function registered(): string[] {
  return listen.mock.calls.map((entry) => entry[0] as string);
}

/** The handler the shell's history event was registered with. */
function navHandler(): (event: { payload: Record<string, unknown> }) => void {
  const call = listen.mock.calls.find((entry) => entry[0] === 'browser_nav_state');
  if (call === undefined) throw new Error('the history listener was never registered');
  return call[1] as (event: { payload: Record<string, unknown> }) => void;
}

describe('the shell listeners', () => {
  /**
   * **The second listener is the one that matters.** The load-time attempt
   * registers the first and is refused by the second; an open must resume
   * from there, registering the second and third and NEVER the first again.
   * A retry that starts from zero shows `lower` twice - which round 2 did.
   */
  it(
    'resumes a failed install from where it stopped, and applies pushed history',
    { timeout: 15000 },
    async () => {
      vi.resetModules();
      listen.mockReset();
      listen.mockResolvedValueOnce(undefined);
      listen.mockRejectedValueOnce(new Error('the event plugin was not ready'));

      // Imported here, not at the top: the module-level install runs inside
      // this test's registry, against the sequence above, deterministically.
      const { closeSurface, openSurface, surface } = await import('./surface.svelte');

      await openSurface(null);
      // A loaded machine can take a moment to run the resumed install; the
      // wait is for the effect, never a fixed sleep.
      await vi.waitFor(() => expect(registered()).toContain('browser_nav_state'), {
        timeout: 5000,
      });
      expect(registered(), 'no listener is registered twice').toEqual([
        'browser_surface_lower',
        'browser_surface_failed',
        'browser_surface_failed',
        'browser_nav_state',
      ]);

      const pushed = navHandler();
      pushed({ payload: { can_back: true, can_forward: false } });
      expect(surface.nav, 'the pushed pair is the bar state').toEqual({
        can_back: true,
        can_forward: false,
      });

      await openSurface(null);
      expect(surface.nav, 'a fresh open starts disabled until the shell answers').toEqual({
        can_back: false,
        can_forward: false,
      });

      pushed({ payload: { can_back: true, can_forward: true } });
      await closeSurface();
      expect(surface.nav, 'a closed surface has nowhere to go').toEqual({
        can_back: false,
        can_forward: false,
      });
    },
  );
});
