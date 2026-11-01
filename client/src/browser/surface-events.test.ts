// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

const { listen, asked } = vi.hoisted(() => ({
  listen: vi.fn(),
  /** Every shell command the page asked for, in order. */
  asked: [] as { command: string; args?: unknown }[],
}));

vi.mock('@tauri-apps/api/event', () => ({ listen }));

import type { Connection } from '../socket';

/**
 * The shell's own side, as the Tauri API calls it: `invoke` goes straight to
 * `window.__TAURI_INTERNALS__.invoke`, so this is the honest seam a page runs
 * against rather than a mock of the package.
 */
function installShellInternals(): void {
  Object.defineProperty(window, '__TAURI_INTERNALS__', {
    configurable: true,
    value: {
      invoke: (command: string, args?: unknown) => {
        asked.push({ command, args });
        if (command === 'client_browser_view') {
          return Promise.resolve({ profile: 'hunt', tab: 2 });
        }
        if (command === 'client_browser_tabs') {
          return Promise.resolve({
            tabs: {
              profile: 'hunt',
              id: 7,
              up: true,
              connected: true,
              closing_at_ms: null,
              rows: [],
            },
          });
        }
        return Promise.resolve(null);
      },
    },
  });
}

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

/** The handler the shell's failure event was registered with. */
function failHandler(): (event: { payload: unknown }) => void {
  const call = listen.mock.calls.find((entry) => entry[0] === 'browser_surface_failed');
  if (call === undefined) throw new Error('the failure listener was never registered');
  return call[1] as (event: { payload: unknown }) => void;
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
      installShellInternals();
      asked.length = 0;
      listen.mockResolvedValueOnce(undefined);
      listen.mockRejectedValueOnce(new Error('the event plugin was not ready'));

      // Imported here, not at the top: the module-level install runs inside
      // this test's registry, against the sequence above, deterministically.
      const { closeSurface, installBrowser, openSurface, surface } =
        await import('./surface.svelte');
      installBrowser({} as Connection);

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
        'browser_keyboard',
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

      // **The shell says whose keyboard it is**, and the page relays the
      // holder as the window event its bar listens for. A payload this page
      // cannot read is no holder at all, rather than a guessed one.
      const keyboard = listen.mock.calls.find((entry) => entry[0] === 'browser_keyboard')?.[1] as
        ((event: { payload: unknown }) => void) | undefined;
      const holders: string[] = [];
      const heard = (event: Event): void => {
        holders.push(event.type);
      };
      window.addEventListener('forge-bar-focus', heard);
      window.addEventListener('forge-page-focus', heard);
      keyboard?.({ payload: { holder: 'bar' } });
      keyboard?.({ payload: { holder: 'page' } });
      keyboard?.({ payload: null });
      keyboard?.({ payload: { holder: 'sideways' } });
      expect(holders, 'the two holders, and nothing from a payload it cannot read').toEqual([
        'forge-bar-focus',
        'forge-page-focus',
      ]);

      // **A failure that names another surface is not this one's.** A late
      // frame for a superseded attach must not fail the tab now viewed.
      const failed = failHandler();
      failed({ payload: { id: 7, tab: 4, reason: 'tab 4 was closed' } });
      expect(surface.phase, 'another tab does not fail this surface').not.toBe('failed');
      failed({ payload: { id: 8, tab: 3, reason: 'tab 3 was closed' } });
      expect(surface.phase, 'another profile does not either').not.toBe('failed');
      failed({ payload: { id: 7, tab: 3, reason: 'the browser host exited' } });
      expect(surface.phase, 'its own failure lands').toBe('failed');
      expect(surface.reason).toBe('the browser host exited');
      expect(
        [surface.id, surface.tab, surface.viewer, surface.controller],
        'the profile id, the tab, this viewer and the role holder',
      ).toEqual([7, 3, 3, 4]);
      expect(surface.attachedProfile, "the server's resolved name is what the bar spells").toBe(
        'browser-stack-lead',
      );

      // **A failure after the surface closed is nobody's**, and one that
      // lands while a new open is still unnamed belongs to the previous
      // attach rather than the opening surface.
      surface.phase = 'closed';
      failed({ payload: { id: 7, tab: 3, reason: 'late' } });
      expect(surface.phase, 'a closed surface stays closed').toBe('closed');
      surface.phase = 'opening';
      surface.id = null;
      surface.tab = null;
      failed({ payload: { id: 7, tab: 3, reason: 'stale' } });
      expect(surface.phase, "an unnamed opening surface is not this failure's to fail").toBe(
        'opening',
      );

      viewed({ payload: { id: 'seven', viewer: 3 } });
      expect(surface.id, 'a value this page cannot read is no id at all').toBeNull();
      expect(surface.controller, 'and a missing role holder is nobody').toBeNull();

      // **The remeasure ask reaches the page.** The shell asks through this
      // listener once a native resize has settled; deleting the relay leaves
      // the overlay laid from the old geometry.
      const remeasure = listen.mock.calls
        .filter((entry) => entry[0] === 'browser_surface_remeasure')
        .at(-1)?.[1] as (() => void) | undefined;
      let remeasured = 0;
      window.addEventListener(
        'forge-surface-remeasure',
        () => {
          remeasured += 1;
        },
        { once: true },
      );
      remeasure?.();
      expect(remeasured, "the shell's remeasure is relayed to the page").toBe(1);

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
      asked.length = 0;
      connection?.();
      await vi.waitFor(() =>
        expect(
          asked.some((call) => call.command === 'client_browser_tabs'),
          'the reconnect re-reads the tabs',
        ).toBe(true),
      );

      await closeSurface();
      expect(surface.nav, 'a closed surface has nowhere to go').toEqual({
        can_back: false,
        can_forward: false,
      });
    },
  );
});

/**
 * **The overlay's two key doors, as the shell wires them.** Both names are
 * literals on each side of the language boundary, so a rename on either side
 * kills desktop dictation silently; the payload comes from a native half with
 * no type link to this page, so its narrowing is pinned here too.
 */
describe("the overlay's key doors", () => {
  it('registers both events and ignores a malformed modifier payload', async () => {
    vi.resetModules();
    listen.mockReset();
    listen.mockResolvedValue(() => undefined);
    installShellInternals();

    const { shellLink } = await import('./shell');
    const presses: { code: number; down: boolean }[] = [];
    const chords: number[] = [];
    await shellLink().onModifier?.((press) => presses.push(press));
    await shellLink().onModifierChord?.(() => chords.push(1));

    expect(registered(), 'the two names the Rust half emits').toEqual([
      'browser_modifier',
      'browser_modifier_chord',
    ]);

    const modifier = listen.mock.calls[0]?.[1] as (event: { payload: unknown }) => void;
    modifier({ payload: { code: 54, down: true } });
    expect(presses, 'a complete payload is the press').toEqual([{ code: 54, down: true }]);
    for (const payload of [
      null,
      undefined,
      'x',
      { code: '54', down: true },
      { code: 54 },
      { code: 54, down: 'true' },
    ]) {
      modifier({ payload });
    }
    expect(presses, 'a malformed payload is ignored').toHaveLength(1);

    const chord = listen.mock.calls[1]?.[1] as (event: { payload: unknown }) => void;
    chord({ payload: null });
    expect(chords, 'the chord carries no payload and is passed on').toEqual([1]);
  });
});

/**
 * **The page names the pane each door moves the keyboard to.** `page` is the
 * native picture and `bar` is this document; swapping the two literals sends
 * the keyboard to the wrong pane with every other test green, so the values
 * are read back off the commands the shell records.
 */
describe('the shell keyboard doors', () => {
  it('asks for the pane each door names', async () => {
    vi.resetModules();
    listen.mockReset();
    listen.mockResolvedValue(() => undefined);
    installShellInternals();
    const { focusBar, focusPage, installBrowserLink } = await import('./link');
    installBrowserLink({} as Connection);
    asked.length = 0;
    await focusPage();
    await focusBar();
    expect(asked, 'the two holders, in the commands the shell receives').toEqual([
      { command: 'browser_surface_focus', args: { holder: 'page' } },
      { command: 'browser_surface_focus', args: { holder: 'bar' } },
    ]);
  });
});
