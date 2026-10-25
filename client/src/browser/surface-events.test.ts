// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

const { listen, listTabs } = vi.hoisted(() => ({
  listen: vi.fn(),
  listTabs: vi.fn(),
}));

vi.mock('@tauri-apps/api/event', () => ({ listen }));
vi.mock('./host', () => ({
  canHost: () => true,
  drawsSurface: () => true,
  showBrowser: () => Promise.resolve({ profile: 'hunt', tab: 2 }),
  hideBrowser: () => Promise.resolve(true),
  listTabs,
  tabListOf: (value: unknown) => value,
  reportSurfaceRect: () => Promise.resolve(),
  resizeBrowser: () => Promise.resolve(),
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

/** The handler the shell's viewer state was registered with. */
function viewHandler(): (event: { payload: Record<string, unknown> }) => void {
  const call = listen.mock.calls.find((entry) => entry[0] === 'browser_view');
  if (call === undefined) throw new Error('the viewer listener was never registered');
  return call[1] as (event: { payload: Record<string, unknown> }) => void;
}

describe('the shell listeners', () => {
  /**
   * **The second listener is the one that matters.** The load-time attempt
   * registers the first and is refused by the second (the remeasure); an open
   * must resume from there, registering the rest and NEVER the first again.
   * A retry that starts from zero shows `lower` twice - which round 2 did.
   */
  it(
    'resumes a failed install from where it stopped, and applies pushed history',
    { timeout: 15000 },
    async () => {
      vi.resetModules();
      listen.mockReset();
      listTabs.mockReset();
      listTabs.mockResolvedValue({
        profile: 'hunt',
        id: 7,
        up: true,
        connected: true,
        closing_at_ms: null,
        rows: [],
      });
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
        'browser_surface_remeasure',
        'browser_surface_remeasure',
        'browser_surface_failed',
        'browser_nav_state',
        'browser_view',
        'browser_tabs',
        'client://connection',
      ]);

      const pushed = navHandler();
      pushed({ payload: { can_back: true, can_forward: false } });
      expect(surface.nav, 'the pushed pair is the bar state').toEqual({
        can_back: true,
        can_forward: false,
      });

      // **The viewer state names the profile and tab a phone's canvas
      // filters on**, and whose role each surface holds; a drift here draws
      // another tab's frame or offers a control the person does not have.
      const viewed = viewHandler();
      viewed({
        payload: { id: 7, profile: 'browser-stack-lead', tab: 3, viewer: 3, controller: 4 },
      });
      expect(
        [surface.id, surface.tab, surface.viewer, surface.controller],
        'the profile id, the tab, this viewer and the role holder',
      ).toEqual([7, 3, 3, 4]);
      expect(surface.attachedProfile, "the server's resolved name is what the bar spells").toBe(
        'browser-stack-lead',
      );
      viewed({ payload: { id: 'seven', viewer: 3 } });
      expect(surface.id, 'a value this page cannot read is no id at all').toBeNull();
      expect(surface.controller, 'and a missing role holder is nobody').toBeNull();

      // **The remeasure ask reaches the page.** The shell refuses a report and
      // asks through this listener; deleting the relay leaves every refusal
      // unanswerable and the surface dark.
      const remeasure = listen.mock.calls
        .filter((entry) => entry[0] === 'browser_surface_remeasure')
        .at(-1)?.[1] as (() => void) | undefined;
      let asked = 0;
      window.addEventListener(
        'forge-surface-remeasure',
        () => {
          asked += 1;
        },
        { once: true },
      );
      remeasure?.();
      expect(asked, "the shell's remeasure is relayed to the page").toBe(1);

      await openSurface(null);
      expect(surface.nav, 'a fresh open starts disabled until the shell answers').toEqual({
        can_back: false,
        can_forward: false,
      });

      pushed({ payload: { can_back: true, can_forward: true } });

      // **A reconnect re-asks the tabs**, so the picker does not freeze on
      // the list the dead socket pushed.
      const connection = listen.mock.calls.find(
        (entry) => entry[0] === 'client://connection',
      )?.[1] as (() => void) | undefined;
      listTabs.mockClear();
      connection?.();
      await vi.waitFor(() => expect(listTabs).toHaveBeenCalledTimes(1));

      await closeSurface();
      expect(surface.nav, 'a closed surface has nowhere to go').toEqual({
        can_back: false,
        can_forward: false,
      });
    },
  );
});
