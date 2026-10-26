// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { canHost, listTabs, onConnection, onTabs } from '../browser/host';
import type { BrowserTabList } from '../protocol';
import type { SessionSlot } from '../wire/types';
import BrowserSegment from './BrowserSegment.svelte';

const { openSurface, closeSurface, surface } = vi.hoisted(() => ({
  openSurface: vi.fn(
    (_profile: string | null, _slot: unknown, _tab: number | null): Promise<string | null> =>
      Promise.resolve(null),
  ),
  closeSurface: vi.fn((_profile: string | null): Promise<string | null> => Promise.resolve(null)),
  // The surface store's fields the strip reads: the raised profile, the
  // server's resolved name, the viewed tab and whether it is open at all.
  surface: {
    phase: 'closed',
    profile: null as string | null,
    attachedProfile: null as string | null,
    tab: null as number | null,
  },
}));
vi.mock('../browser/surface.svelte', () => ({ openSurface, closeSurface, surface }));

const SLOT: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** One profile's tabs, as the shell answers the read. */
function tabs(over: Partial<BrowserTabList> = {}): BrowserTabList {
  return {
    profile: 'TestOrg/work',
    id: 3,
    up: true,
    connected: true,
    closing_at_ms: null,
    rows: [],
    ...over,
  };
}

/** One tab row, with the server's own fields; an override may be unreadable. */
function row(over: Record<string, unknown> = {}): BrowserTabList['rows'][number] {
  return {
    number: 1,
    title: 'portal',
    url: 'https://x/',
    owner: { org: 'TestOrg', project: 'proj', label: 'lead' },
    state: 'idle',
    ...over,
  };
}

afterEach(() => {
  vi.mocked(canHost).mockReturnValue(true);
  vi.mocked(listTabs).mockResolvedValue(tabs());
  surface.phase = 'closed';
  surface.profile = null;
  surface.attachedProfile = null;
  surface.tab = null;
  openSurface.mockClear();
  closeSurface.mockClear();
  // **The panel is PORTALED to the body**, so a test that ends with one
  // open would hand it to the next - and the next test's `list()` would read
  // the stale one.
  document.body.innerHTML = '';
});

/** The shell's viewer, mocked so the segment's reads have states to drive. */
vi.mock('../browser/host', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/host')>();
  return {
    ...actual,
    canHost: vi.fn(() => true),
    listTabs: vi.fn(() =>
      Promise.resolve({
        profile: 'TestOrg/work',
        id: 3,
        up: true,
        connected: true,
        closing_at_ms: null,
        rows: [],
      }),
    ),
    onTabs: vi.fn(() => Promise.resolve(() => {})),
    onConnection: vi.fn(() => Promise.resolve(() => {})),
    drawsSurface: vi.fn(() => true),
  };
});

/** One mounted segment, and a function that takes it down. */
function show(slot: SessionSlot | null = null) {
  const target = document.createElement('div');
  document.body.appendChild(target);
  const app = mount(BrowserSegment, { target, props: { slot } });
  flushSync();
  return { target, stop: () => void unmount(app) };
}

const toggle = (target: HTMLElement): HTMLButtonElement => {
  const button = target.querySelector<HTMLButtonElement>('.bz-tog');
  if (button === null) throw new Error('no toggle');
  return button;
};

const list = (): HTMLElement | null => document.body.querySelector('.bz-list');

describe('the browser segment', () => {
  it('rests on the tab count and opens onto the profile', async () => {
    const shown = show();
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .n')?.textContent,
        'the resting row names the count once a read has answered',
      ).toBe('0 tabs');
    });

    toggle(shown.target).click();
    flushSync();
    expect(list(), 'the row opens a panel').not.toBeNull();
    expect(list()?.textContent, 'the resolved profile is named').toContain('TestOrg/work');
    await vi.waitFor(() => {
      expect(list()?.textContent, 'and no tabs says so plainly').toContain('no tabs yet');
    });
    shown.stop();
  });

  /// **The rows are the server's answer**, drawn with the tabs' own states:
  /// a call in flight spins the work ring, a parked hand-off marks the held
  /// tab, and an idle one carries the idle disc.
  it("draws the server's tabs with their numbers, owners and states", async () => {
    vi.mocked(listTabs).mockResolvedValue(
      tabs({
        rows: [
          row({ number: 1, title: 'portal', url: 'https://x/', state: 'in_use' }),
          row({
            number: 2,
            title: '',
            url: 'about:blank',
            owner: { org: 'TestOrg', project: 'proj', label: 'w1' },
            state: 'held',
          }),
          row({ number: 3, title: 'docs', state: 'idle' }),
        ],
      }),
    );
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('3 tabs'),
    );
    toggle(shown.target).click();
    flushSync();
    const rows = [...(list()?.querySelectorAll('.bz-it') ?? [])];
    const driven = rows.find((entry) => entry.textContent?.includes('tab 1'));
    expect(driven?.querySelector('.ring'), 'a call in flight spins the work ring').not.toBeNull();
    expect(driven?.textContent, 'and the row names its owner').toContain('TestOrg/proj/lead');
    expect(driven?.textContent, 'and the state in the server words').toContain('in use');
    const held = rows.find((entry) => entry.textContent?.includes('tab 2'));
    expect(held?.querySelector('.dot.warn'), 'a held tab carries its own mark').not.toBeNull();
    // **A tab with no title reads as its address**, not as an empty cell.
    expect(held?.textContent, 'the address stands in for a missing title').toContain('about:blank');
    const idle = rows.find((entry) => entry.textContent?.includes('tab 3'));
    expect(idle?.querySelector('.dot.idle'), 'an idle tab carries the idle disc').not.toBeNull();
    shown.stop();
  });

  /// **A row with no owner is refused, not drawn under an invented one**: a
  /// tab belongs to the session whose calls drive it, and a row that cannot
  /// say whose it is fails the read with the reason.
  it('fails the read when a tab names no owner', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ owner: null })] }));
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('count unknown'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list()?.textContent, 'the reason is drawn, in its own words').toContain(
      'names no owner',
    );
    shown.stop();
  });

  /// **Show raises the takeover on that tab of the profile**, and the row
  /// carries the tab number so the page lands on the tab the person picked.
  it('raises the takeover for the seat and the tab the row names', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ number: 4 })] }));
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    toggle(shown.target).click();
    flushSync();
    const showIt = list()?.querySelector<HTMLButtonElement>('.bz-show');
    if (showIt == null) throw new Error('no show control');
    showIt.click();
    await vi.waitFor(() => expect(openSurface).toHaveBeenCalledWith('TestOrg/work', SLOT, 4));
    shown.stop();
  });

  /// **A tab this page already fronts says hide**, and its own lower is the
  /// surface's, not the server's.
  it('lowers the tab this client already fronts', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ number: 4 })] }));
    surface.phase = 'live';
    surface.profile = 'TestOrg/work';
    surface.attachedProfile = 'TestOrg/work';
    surface.tab = 4;
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    toggle(shown.target).click();
    flushSync();
    const hide = list()?.querySelector<HTMLButtonElement>('[aria-label="hide tab 4"]');
    expect(hide?.textContent?.trim()).toBe('hide');
    hide?.click();
    await vi.waitFor(() => expect(closeSurface).toHaveBeenCalledWith('TestOrg/work', SLOT));
    shown.stop();
  });

  /// **Another client's tab is not this client's hide.** The state is the
  /// server's, and which tab this page paints is its own.
  it('offers show for a tab another client fronts', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ number: 4 })] }));
    surface.phase = 'live';
    surface.profile = 'TestOrg/work';
    surface.attachedProfile = 'TestOrg/work';
    surface.tab = 1;
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    toggle(shown.target).click();
    flushSync();
    const showIt = list()?.querySelector<HTMLButtonElement>('[aria-label^="show tab 4"]');
    expect(showIt?.textContent?.trim(), 'this client is not showing it').toBe('show');
    shown.stop();
  });

  /// **The strip carries no profile verbs**: a profile is not closed or
  /// deleted from here, and a tab is not created - it belongs to the calls
  /// that drive it.
  it('carries no close or delete verb', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ number: 4 })] }));
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list()?.querySelector('.bz-close'), 'no close verb').toBeNull();
    expect(list()?.querySelector('.bz-del'), 'no delete verb').toBeNull();
    shown.stop();
  });

  it('collapses on the toggle and returns focus on Escape', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row()] }));
    const shown = show();
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list(), 'opened').not.toBeNull();

    toggle(shown.target).click();
    flushSync();
    expect(list(), 'and the same click closes it').toBeNull();

    toggle(shown.target).click();
    flushSync();
    const control = list()?.querySelector<HTMLButtonElement>('.bz-show') ?? null;
    if (control === null) throw new Error('no control');
    control.focus();
    control.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape closes the panel').toBeNull();
    expect(document.activeElement, 'and the toggle takes the focus').toBe(toggle(shown.target));
    shown.stop();
  });

  /// **The hover names the resolved profile and how it stands**, and the
  /// idle deadline reads as the minutes the person has.
  it('names the profile, the connection and the closing time', async () => {
    vi.mocked(listTabs).mockResolvedValue(
      tabs({
        connected: false,
        closing_at_ms: Date.now() + 90_000,
        rows: [row()],
      }),
    );
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog')?.getAttribute('title')).toContain(
        'TestOrg/work · the host is up, no driver attached · closing in 2m',
      ),
    );
    shown.stop();
  });

  /// **The mark is the server's own row**, not this client's attachment:
  /// the phone holds no attachment of its own and still shows it.
  it('says a connected browser on the resting row', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs());
    const shown = show();
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .dot.ok'),
        'a connected browser carries the ok disc',
      ).not.toBeNull();
    });
    shown.stop();
  });

  /// **A refused raise is drawn, and a read landing behind it does not erase
  /// the sentence.**
  it('draws a refused raise and keeps it when a read lands behind it', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ number: 4 })] }));
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    toggle(shown.target).click();
    flushSync();

    // A read is in flight when the raise is refused.
    let land: (answer: BrowserTabList) => void = () => {};
    vi.mocked(listTabs).mockReturnValueOnce(
      new Promise((resolve) => {
        land = resolve;
      }),
    );
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    flushSync();
    openSurface.mockResolvedValueOnce('the browser host is gone');
    list()?.querySelector<HTMLButtonElement>('.bz-show')?.click();
    await vi.waitFor(() =>
      expect(list()?.textContent, "the shell's own words").toContain('the browser host is gone'),
    );

    land(tabs({ rows: [row({ number: 4 })] }));
    // **Let the stale read land, then render it**: without the flush the
    // assertion reads the previous DOM and passes whatever the guard did.
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(list()?.textContent, 'the stale read did not erase the refusal').toContain(
      'the browser host is gone',
    );
    shown.stop();
  });

  /// **The engine line names what runs the page, and says so when nothing
  /// does**, on both platforms.
  it('names the engine behind the page and says when there is none', async () => {
    vi.mocked(listTabs).mockResolvedValue(tabs({ up: false, connected: false, rows: [] }));
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('0 tabs'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list()?.querySelector('.bz-engine')?.textContent).toBe('nothing to run a page');
    expect(list()?.textContent, 'and no host is up').toContain('no host up');

    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row()] }));
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    expect(list()?.querySelector('.bz-engine')?.textContent).toBe("the server's own Chromium");
    shown.stop();
  });

  /// **A pushed list moves the panel without a poll**, which is how a tab
  /// that opens or closes under the person arrives.
  it('follows a pushed tab list', async () => {
    let push: (answer: BrowserTabList) => void = () => {};
    vi.mocked(onTabs).mockImplementation((fn) => {
      push = fn;
      return Promise.resolve(() => {});
    });
    vi.mocked(listTabs).mockResolvedValue(tabs());
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('0 tabs'),
    );

    push(tabs({ rows: [row({ number: 2 })] }));
    flushSync();
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    shown.stop();
  });

  /// **A reconnect re-reads the tabs.** The push subscription lived on the
  /// socket that dropped; without the read the strip froze on its last list.
  it('re-reads the tabs when the connection is new again', async () => {
    const seen: { notice: (() => void) | null } = { notice: null };
    vi.mocked(onConnection).mockImplementation((fn) => {
      seen.notice = fn;
      return Promise.resolve(() => {});
    });
    vi.mocked(listTabs).mockResolvedValue(tabs());
    const shown = show(SLOT);
    await vi.waitFor(() => expect(listTabs).toHaveBeenCalledTimes(1));

    vi.mocked(listTabs).mockClear();
    vi.mocked(listTabs).mockResolvedValue(tabs({ rows: [row({ number: 3 })] }));
    seen.notice?.();
    await vi.waitFor(() => expect(listTabs).toHaveBeenCalledTimes(1));
    flushSync();
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 tab'),
    );
    shown.stop();
  });
});
