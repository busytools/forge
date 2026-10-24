// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { canHost, closeProfile, deleteProfile, hideBrowser, listProfiles } from '../browser/host';
import type { SessionSlot } from '../wire/types';
import BrowserSegment from './BrowserSegment.svelte';

const { openSurface, closeSurface, surface } = vi.hoisted(() => ({
  openSurface: vi.fn((_profile: string | null, _slot: unknown): Promise<string | null> =>
    Promise.resolve(null),
  ),
  closeSurface: vi.fn((_profile: string | null): Promise<string | null> => Promise.resolve(null)),
  // The surface store's two fields the strip reads: this client's open
  // profile (null is the shared browser) and whether it is open at all.
  surface: { phase: 'closed', profile: null as string | null },
}));
vi.mock('../browser/surface.svelte', () => ({ openSurface, closeSurface, surface }));

const SLOT: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

afterEach(() => {
  vi.mocked(canHost).mockReturnValue(true);
  vi.mocked(listProfiles).mockImplementation(() => Promise.resolve([]));
  vi.mocked(hideBrowser).mockClear();
  vi.mocked(closeProfile).mockClear();
  vi.mocked(deleteProfile).mockReset();
  vi.mocked(deleteProfile).mockResolvedValue(undefined);
  surface.phase = 'closed';
  surface.profile = null;
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
    listProfiles: vi.fn(() => Promise.resolve([])),
    hideBrowser: vi.fn(() => Promise.resolve({ lowered: true })),
    closeProfile: vi.fn(() => Promise.resolve()),
    deleteProfile: vi.fn(() => Promise.resolve()),
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
  it('rests on the count and opens onto the shared browser', async () => {
    const shown = show();
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .n')?.textContent,
        'the resting row names the count once a read has answered',
      ).toBe('0 profiles');
    });

    toggle(shown.target).click();
    flushSync();
    expect(list(), 'the row opens a panel').not.toBeNull();
    expect(list()?.textContent, 'the shared browser is always listed').toContain('shared');
    await vi.waitFor(() => {
      expect(list()?.textContent, 'and no named profiles says so plainly').toContain(
        'no named profiles yet',
      );
    });
    shown.stop();
  });

  it('shows the shared browser from its row', async () => {
    const shown = show();
    toggle(shown.target).click();
    flushSync();
    const showIt = list()?.querySelector<HTMLButtonElement>('.bz-show');
    if (showIt == null) throw new Error('no show control');
    showIt.click();
    flushSync();
    await vi.waitFor(() => expect(openSurface).toHaveBeenCalledWith('@shared', null));
    shown.stop();
  });

  /// **The raise names THIS seat.** The server attaches the viewer to a
  /// slot's browser; a raise that named none was refused and left the
  /// surface dark under its bar (measured on the dev stack).
  it('raises the takeover for the seat the strip belongs to', async () => {
    openSurface.mockClear();
    const shown = show(SLOT);
    toggle(shown.target).click();
    flushSync();
    const showIt = list()?.querySelector<HTMLButtonElement>('.bz-show');
    if (showIt == null) throw new Error('no show control');
    showIt.click();
    await vi.waitFor(() => expect(openSurface).toHaveBeenCalledWith('@shared', SLOT));
    shown.stop();
  });

  it('collapses on the toggle and returns focus on Escape', () => {
    const shown = show();
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

  it('says a driven browser on the resting row', async () => {
    // **The mark is the server's own row**, not this client's attachment:
    // the phone holds no attachment of its own and still shows it.
    vi.mocked(listProfiles).mockResolvedValue([
      { name: '@shared', owner: '', up: true, windowed: false, busy: false, closed: false },
    ]);
    const shown = show();
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .dot.idle'),
        'a browser whose host is up carries the idle disc',
      ).not.toBeNull();
    });
    shown.stop();
  });

  /// **The rows are the server's answer**, drawn with the states they carry:
  /// a closed profile says its logins are still here, a running one shows
  /// the idle disc, and one with a call in flight spins the work ring.
  it("draws the server's rows with their own states", async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: '@shared', owner: '', up: true, windowed: false, busy: false, closed: false },
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: true,
        closed: false,
      },
      { name: 'old', owner: '', up: false, windowed: false, busy: false, closed: true },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('3 profiles'),
    );
    toggle(shown.target).click();
    flushSync();
    const rows = [...(list()?.querySelectorAll('.bz-it') ?? [])];
    const hunt = rows.find((row) => row.textContent?.includes('hunt'));
    expect(hunt?.querySelector('.ring'), 'a call in flight spins the work ring').not.toBeNull();
    expect(hunt?.textContent, 'and the row names its owner').toContain('TestOrg/proj/lead');
    expect(list()?.textContent, 'a closed row says its logins stay').toContain(
      'closed · its logins are still here',
    );
    shown.stop();
  });

  /// **A row the server sent that this page cannot read is drawn as itself**,
  /// with no verbs: the count and the empty line must not report a state the
  /// server never sent.
  it('draws an unreadable row instead of dropping it', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: '(unreadable 1)',
        owner: '',
        up: false,
        windowed: false,
        busy: false,
        closed: false,
        unreadable: true,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list()?.textContent).toContain('its owner record could not be read');
    expect(
      list()?.querySelector('[aria-label*="(unreadable 1)"]'),
      'an unreadable row carries no verb',
    ).toBeNull();
    shown.stop();
  });

  /// **A close is the server's ask, for this seat**, and the strip re-reads
  /// so the row goes when the server says it did.
  it('closes a named profile through the server and re-reads', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    const close = list()?.querySelector<HTMLButtonElement>('.bz-close');
    if (close == null) throw new Error('no close control');
    close.click();
    await vi.waitFor(() => expect(closeProfile).toHaveBeenCalledWith('hunt', SLOT));
    shown.stop();
  });

  /// **A delete confirms in the row, then asks the server for the name and
  /// this seat**, and the outcome line says what went.
  it('deletes a profile after its own confirmation', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del')?.click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del.confirm')?.click();
    await vi.waitFor(() => expect(deleteProfile).toHaveBeenCalledWith('hunt', SLOT));
    await vi.waitFor(() =>
      expect(list()?.textContent, 'the outcome names what went').toContain(
        "deleted the 'hunt' profile",
      ),
    );
    shown.stop();
  });

  /// **A delete marks its row while the shell works**, so a slow delete is
  /// not a dead button: the row says what is happening until it answers.
  it('marks the row while its delete runs', async () => {
    let finish: () => void = () => {};
    vi.mocked(deleteProfile).mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del')?.click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del.confirm')?.click();
    await vi.waitFor(() => expect(list()?.textContent).toContain('deleting...'));
    finish();
    await vi.waitFor(() =>
      expect(list()?.textContent, 'the outcome replaces the mark').toContain(
        "deleted the 'hunt' profile",
      ),
    );
    shown.stop();
  });

  /// **A single delete the shell refuses draws its own sentence**, and the
  /// profile stands: the panel says why rather than closing over a refusal.
  it('draws a refused single delete', async () => {
    vi.mocked(deleteProfile).mockRejectedValueOnce('the profile is held by another seat');
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del')?.click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del.confirm')?.click();
    await vi.waitFor(() => expect(deleteProfile).toHaveBeenCalledWith('hunt', SLOT));
    await vi.waitFor(() =>
      expect(list()?.textContent, "the shell's own words").toContain('held by another seat'),
    );
    shown.stop();
  });

  /// **The bulk delete is one ask per closed profile, never a verb of its
  /// own**, and its summary counts what went.
  it('deletes every closed profile, one ask per row', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
      {
        name: 'old',
        owner: '',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('2 profiles'),
    );
    toggle(shown.target).click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del-all')?.click();
    flushSync();
    expect(list()?.textContent, 'the confirm lists every closed name').toContain('hunt');
    expect(list()?.textContent, 'including the ownerless one').toContain('old');
    list()?.querySelector<HTMLButtonElement>('.bz-del.confirm')?.click();
    await vi.waitFor(() => expect(deleteProfile).toHaveBeenCalledTimes(2));
    expect(vi.mocked(deleteProfile).mock.calls, 'each row asked for itself').toEqual([
      ['hunt', SLOT],
      ['old', SLOT],
    ]);
    await vi.waitFor(() =>
      expect(list()?.textContent).toContain('deleted 2 closed profiles and their saved logins'),
    );
    shown.stop();
  });

  /// **A bulk delete that is partly refused keeps its reason on the row that
  /// stayed**, because a sibling's re-read clears the panel's own line.
  it('marks the closed row a bulk delete could not take', async () => {
    vi.mocked(deleteProfile).mockImplementation((name) =>
      name === 'old'
        ? Promise.reject(new Error('the profile is held by another seat'))
        : Promise.resolve(),
    );
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
      {
        name: 'old',
        owner: '',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('2 profiles'),
    );
    toggle(shown.target).click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del-all')?.click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del.confirm')?.click();
    await vi.waitFor(() =>
      expect(list()?.textContent, 'the summary counts only what went').toContain(
        'deleted 1 closed profile and its saved logins',
      ),
    );
    await vi.waitFor(() =>
      expect(list()?.textContent, 'the refusal stays on the row that stayed').toContain(
        'held by another seat',
      ),
    );
    shown.stop();
  });

  /// **A closed profile has no run to close**: the row carries its delete,
  /// and a close there would ask the server to end a browser that is not up.
  it('offers no close on a closed profile', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list()?.querySelector('.bz-close'), 'no close verb on a closed row').toBeNull();
    shown.stop();
  });

  /// **A row that is up as the takeover says hide**, and its own lower is
  /// the surface's, not the server's.
  it('lowers a profile that is already up', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: true,
        busy: false,
        closed: false,
      },
    ]);
    surface.phase = 'live';
    surface.profile = 'hunt';
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    const hide = list()?.querySelector<HTMLButtonElement>(
      '[aria-label="hide the hunt profile\'s browser"]',
    );
    expect(hide?.textContent?.trim()).toBe('hide');
    hide?.click();
    await vi.waitFor(() => expect(closeSurface).toHaveBeenCalledWith('hunt', SLOT));
    shown.stop();
  });

  /// **Another client's takeover is not this client's hide.** The server's
  /// windowed flag is fleet-wide, so a row read from it offered hide on a
  /// phone that was not showing anything (measured on the dev stack).
  it('offers show for a profile only another client has up', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: true,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    const showIt = list()?.querySelector<HTMLButtonElement>(
      '[aria-label="show the hunt profile\'s browser"]',
    );
    expect(showIt?.textContent?.trim(), 'this client is not showing it').toBe('show');
    showIt?.click();
    await vi.waitFor(() => expect(openSurface).toHaveBeenCalledWith('hunt', SLOT));
    shown.stop();
  });

  /// The shared browser's own row follows the same rule.
  it('offers show for the shared browser another client has up', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: '@shared',
        owner: '',
        up: true,
        windowed: true,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();
    const showIt = list()?.querySelector<HTMLButtonElement>('[aria-label="show the browser"]');
    expect(showIt?.textContent?.trim(), "the shared browser is not this client's open").toBe(
      'show',
    );
    showIt?.click();
    await vi.waitFor(() => expect(openSurface).toHaveBeenCalledWith('@shared', SLOT));
    shown.stop();
  });

  /// **The bulk confirm deletes the names it showed**, not whatever the
  /// latest read happens to call closed: a sibling read landing between the
  /// arming and the confirm must not shrink or grow the set the person
  /// agreed to.
  it('deletes the names the bulk confirm showed, even if a read lands behind it', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
      { name: 'old', owner: '', up: false, windowed: false, busy: false, closed: true },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('2 profiles'),
    );
    toggle(shown.target).click();
    flushSync();
    list()?.querySelector<HTMLButtonElement>('.bz-del-all')?.click();
    flushSync();
    expect(list()?.textContent, 'the confirm lists both').toContain('old');

    // A read lands after the arming and drops one of the chosen names.
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: false,
        windowed: false,
        busy: false,
        closed: true,
      },
    ]);
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );

    list()?.querySelector<HTMLButtonElement>('.bz-del.confirm')?.click();
    await vi.waitFor(() => expect(deleteProfile).toHaveBeenCalledTimes(2));
    expect(vi.mocked(deleteProfile).mock.calls, 'the set the person agreed to').toEqual([
      ['hunt', SLOT],
      ['old', SLOT],
    ]);
    shown.stop();
  });

  /// **A refused close is drawn, and an opening's read landing behind it
  /// does not erase the sentence.** The same guard the delete and the lower
  /// carry.
  it('draws a refused close and keeps it when a read lands behind it', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: false,
        closed: false,
      },
    ]);
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();

    // A read is in flight when the close is refused.
    let land: (rows: Awaited<ReturnType<typeof listProfiles>>) => void = () => {};
    vi.mocked(listProfiles).mockReturnValueOnce(
      new Promise((resolve) => {
        land = resolve;
      }),
    );
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    flushSync();
    vi.mocked(closeProfile).mockRejectedValueOnce(new Error('the profile is held by another seat'));
    list()?.querySelector<HTMLButtonElement>('.bz-close')?.click();
    await vi.waitFor(() =>
      expect(list()?.textContent, "the shell's own words").toContain('held by another seat'),
    );

    land([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: false,
        busy: false,
        closed: false,
      },
    ]);
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(list()?.textContent, 'the stale read did not erase the refusal').toContain(
      'held by another seat',
    );
    shown.stop();
  });

  /// **A refused lower is drawn, and an opening's read landing behind it
  /// does not erase the sentence.** The same guard the close and the delete
  /// carry, through the third path.
  it('draws a refused lower and keeps it when a read lands behind it', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: true,
        busy: false,
        closed: false,
      },
    ]);
    surface.phase = 'live';
    surface.profile = 'hunt';
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    toggle(shown.target).click();
    flushSync();

    let land: (rows: Awaited<ReturnType<typeof listProfiles>>) => void = () => {};
    vi.mocked(listProfiles).mockReturnValueOnce(
      new Promise((resolve) => {
        land = resolve;
      }),
    );
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    flushSync();
    vi.mocked(closeSurface).mockResolvedValueOnce('the browser host is gone');
    // **The row's own button**, not the shared row's `bz-show` that comes
    // first in the panel.
    list()
      ?.querySelector<HTMLButtonElement>('[aria-label="hide the hunt profile\'s browser"]')
      ?.click();
    await vi.waitFor(() =>
      expect(list()?.textContent, "the shell's own words").toContain('the browser host is gone'),
    );
    expect(closeSurface, 'the lower is the path driven').toHaveBeenCalledWith('hunt', SLOT);

    land([
      {
        name: 'hunt',
        owner: 'TestOrg/proj/lead',
        up: true,
        windowed: true,
        busy: false,
        closed: false,
      },
    ]);
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
    const shown = show(SLOT);
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('0 profiles'),
    );
    toggle(shown.target).click();
    flushSync();
    expect(list()?.querySelector('.bz-engine')?.textContent).toBe('nothing to run a page');
    expect(list()?.textContent, 'and no host is up').toContain('no browser is up');

    vi.mocked(listProfiles).mockResolvedValue([
      { name: '@shared', owner: '', up: true, windowed: false, busy: false, closed: false },
    ]);
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    await vi.waitFor(() =>
      expect(shown.target.querySelector('.bz-tog .n')?.textContent).toBe('1 profile'),
    );
    expect(list()?.querySelector('.bz-engine')?.textContent).toBe("the server's own Chromium");
    expect(list()?.textContent, 'and the host is named').toContain("the server's browser is up");
    shown.stop();
  });
});
