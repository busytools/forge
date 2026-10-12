// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import {
  browserUsed,
  closeProfile,
  deleteProfile,
  hideBrowser,
  listProfiles,
  profileWindowed,
  showBrowser,
} from '../browser/host';
import BrowserSegment from './BrowserSegment.svelte';

afterEach(() => {
  vi.mocked(showBrowser).mockClear();
  vi.mocked(hideBrowser).mockClear();
  vi.mocked(profileWindowed).mockClear();
  vi.mocked(deleteProfile).mockClear();
  // **The panel is PORTALED to the body** (`use:portal`), so a test that ends
  // with one open would hand it to the next - and the next test's `list()`
  // would read the stale one. The mount targets live under the body too.
  document.body.innerHTML = '';
});

/**
 * The client's own host, mocked so the READ has states a test can drive: the
 * real one answers the empty list outside the shell, which only ever proved
 * the resting row.
 */
vi.mock('../browser/host', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/host')>();
  return {
    ...actual,
    canHost: () => true,
    listProfiles: vi.fn(() => Promise.resolve([])),
    closeProfile: vi.fn(() => Promise.resolve(undefined)),
    deleteProfile: vi.fn(() => Promise.resolve(undefined)),
    browserUsed: vi.fn(() => Promise.resolve(false)),
    showBrowser: vi.fn(() => Promise.resolve(null)),
    hideBrowser: vi.fn(() => Promise.resolve(null)),
    profileWindowed: vi.fn(() => Promise.resolve(false)),
  };
});

/**
 * The connection the segment reads its role from and takes it through: a
 * stand-in for the two frames the server sends (the grant, and the loss to a
 * force-take).
 */
function fakeConnection(hosting = false) {
  const hearers = new Set<(now: boolean) => void>();
  const swept = new Set<() => void>();
  let role = hosting;
  const taken = vi.fn();
  const connection = {
    browserRole: () => role,
    onBrowserRole: (fn: (now: boolean) => void) => {
      hearers.add(fn);
      return () => hearers.delete(fn);
    },
    onProfilesChanged: (fn: () => void) => {
      swept.add(fn);
      return () => swept.delete(fn);
    },
    takeBrowserRole: taken,
  } as unknown as Connection;
  return {
    connection,
    /** The server saying the role changed. */
    flip: (now: boolean) => {
      role = now;
      for (const hear of hearers) hear(now);
    },
    /** The host saying it swept; every open strip re-reads. */
    swept: () => {
      for (const hear of swept) hear();
    },
    taken,
  };
}

/** The segment, mounted with whatever role, capability and slot the test names. */
function show(hosting = false, capable = true, slot: SessionSlot | null = null) {
  const harness = fakeConnection(hosting);
  const target = document.createElement('div');
  document.body.append(target);
  const app = mount(BrowserSegment, {
    target,
    props: { connection: harness.connection, capable, slot },
  });
  flushSync();
  return { ...harness, target, stop: () => void unmount(app) };
}

const click = (el: Element | null | undefined): void => {
  if (!(el instanceof HTMLElement)) throw new Error('nothing to click');
  el.click();
  flushSync();
};

/** The toggle, where the family's pointer handling lives. */
function toggle(target: HTMLElement): HTMLButtonElement {
  const el = target.querySelector<HTMLButtonElement>('.bz-tog');
  if (el === null) throw new Error('no toggle');
  return el;
}

function list(target: HTMLElement): Element | null {
  // **The panel is PORTALED to the body** (`use:portal`), so it is not a
  // descendant of the mounted segment - and a query scoped to the mount would
  // read every open panel as absent.
  void target;
  return document.body.querySelector('.bz-list');
}

/** What the segment shows: its own text plus the PORTALED panel's. */
function shownText(target: HTMLElement): string {
  const panel = document.body.querySelector('.bz-list');
  return `${target.textContent ?? ''}${panel?.textContent ?? ''}`;
}

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe('the browser segment', () => {
  it('rests as a count and opens onto the role and the profiles', async () => {
    const shown = show();
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .n')?.textContent,
        'the resting row names the count once a read has answered',
      ).toBe('0 profiles');
    });
    expect(shownText(shown.target), 'and nothing about the role until it is open').not.toContain(
      'browser connected',
    );

    click(shown.target.querySelector('.bz-tog'));

    expect(shownText(shown.target), 'a capable client that is not hosting is told so').toContain(
      'browser not connected',
    );
    expect(
      shownText(shown.target),
      'the shared profile is always listed: most sessions drive it',
    ).toContain('shared');
    await vi.waitFor(() => {
      expect(
        shownText(shown.target),
        'with no NAMED profiles said plainly, once the read has answered',
      ).toContain('no named profiles yet');
    });
    shown.stop();
  });

  it('waits on the count and the list until a read has answered', () => {
    // A read that has not answered: neither surface may claim a state.
    vi.mocked(listProfiles).mockImplementation(() => new Promise<never>(() => undefined));
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));

    expect(
      shown.target.querySelector('.bz-tog .n')?.textContent,
      'the count is not known before a read answers',
    ).toBe('…');
    expect(shownText(shown.target), 'and the list says it is reading').toContain(
      'reading the profiles…',
    );
    expect(
      shownText(shown.target),
      'an empty row would be a claim about a read that never answered',
    ).not.toContain('no named profiles yet');
    shown.stop();
    vi.mocked(listProfiles).mockImplementation(() => Promise.resolve([]));
  });

  /** Ved, live round 2026-10-07: the row should say whether the browser has
   *  been actively used - a flat disc beside the glyph when it has. */
  it('marks the row once the browser has been used, and not before', async () => {
    const resting = show(false, true);
    await vi.waitFor(() => expect(browserUsed).toHaveBeenCalled());
    expect(
      resting.target.querySelector('.bz-tog .dot.idle'),
      'a browser nobody has driven carries no mark',
    ).toBeNull();
    resting.stop();

    vi.mocked(browserUsed).mockResolvedValueOnce(true);
    const used = show(false, true);
    await vi.waitFor(() => {
      expect(
        used.target.querySelector('.bz-tog .dot.idle'),
        'a driven browser says so on the resting row',
      ).not.toBeNull();
    });
    used.stop();
    vi.mocked(browserUsed).mockResolvedValue(false);
  });

  /** Ved, live round 2026-10-07: hovering the row must offer the way to SEE
   *  the browser - the real window, with no session answering anything. */
  it('raises the browser window from the shared row', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));

    const door = list(shown.target)?.querySelector('.bz-show');
    expect(door, 'the shared row carries the way to look').not.toBeNull();
    click(door);

    expect(showBrowser, 'the raise the door asks for').toHaveBeenCalledTimes(1);
    shown.stop();
  });

  /** The measured bug: the phone's takeover bar lowers on Done as well as on
   *  its back, and a re-read left on `lowered` alone kept the row saying
   *  "hide" over a window already down. Every bar word re-reads. */
  it("re-reads the shared row on the takeover bar's own words", async () => {
    const shown = show(false, true);
    await vi.waitFor(() => expect(profileWindowed).toHaveBeenCalledTimes(1));
    const before = vi.mocked(profileWindowed).mock.calls.length;

    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'done' }));
    await vi.waitFor(() => {
      expect(
        vi.mocked(profileWindowed).mock.calls.length,
        'Done lowers the window too; the row must re-read on its word',
      ).toBeGreaterThan(before);
    });

    const afterDone = vi.mocked(profileWindowed).mock.calls.length;
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    await vi.waitFor(() => {
      expect(
        vi.mocked(profileWindowed).mock.calls.length,
        'the raise lands on the UI thread after a caller read; its word re-reads too',
      ).toBeGreaterThan(afterDone);
    });

    // **`lowered` is now the only word the bar's back sends** - dropping
    // that arm left every test green before this case existed.
    const afterRaised = vi.mocked(profileWindowed).mock.calls.length;
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'lowered' }));
    await vi.waitFor(() => {
      expect(
        vi.mocked(profileWindowed).mock.calls.length,
        "the bar's back says lowered, and nothing else - the row must re-read on it",
      ).toBeGreaterThan(afterRaised);
    });
    shown.stop();
  });

  it("keeps the panel and says the shell's own reason when the window cannot open", async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce('no browser to drive: install Brave');
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    click(list(shown.target)?.querySelector('.bz-show'));

    await vi.waitFor(() => {
      expect(shownText(shown.target)).toContain('no browser to drive: install Brave');
    });
    expect(list(shown.target), 'the row stays for a door that failed').not.toBeNull();
    shown.stop();
  });

  it('says the read failed rather than claiming there are no profiles', async () => {
    const shown = show(false, true);
    await vi.waitFor(() => expect(listProfiles).toHaveBeenCalledTimes(1));
    vi.mocked(listProfiles).mockRejectedValueOnce('the profile list would not read');

    click(shown.target.querySelector('.bz-tog'));

    await vi.waitFor(() => {
      expect(shownText(shown.target)).toContain('the profile list would not read');
    });
    expect(
      shownText(shown.target),
      'an empty row would be a claim about a read that never answered',
    ).not.toContain('no named profiles yet');
    shown.stop();
  });

  it('keeps the row and says why when the close is refused', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));
    // **One is singular.** The count arm has a `=== 1 ? '' : 's'` that
    // nothing pinned, and "1 profiles" is what a reader sees when it goes.
    expect(
      shown.target.querySelector('.bz-tog .n')?.textContent,
      'one profile reads singular',
    ).toBe('1 profile');

    // **The close names the ROW's own profile**, not its owner's slot: the
    // two strings sit beside each other on the row, and a swap would strand
    // exactly the profile whose owner is gone - the one this door exists for.
    vi.mocked(closeProfile).mockRejectedValueOnce('no browser profile is open under hunt');
    click(list(shown.target)?.querySelector('.bz-close'));

    expect(closeProfile, 'the name the row carries is what is closed').toHaveBeenCalledWith('hunt');
    await vi.waitFor(() => {
      expect(shownText(shown.target)).toContain('no browser profile is open under hunt');
    });
    expect(shownText(shown.target), 'the row is not taken away by a close that failed').toContain(
      'hunt',
    );
    shown.stop();
  });

  it('arms the delete in the row itself, and keep takes it back', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    click(list(shown.target)?.querySelector('.bz-del'));
    expect(shownText(shown.target), 'the warning takes the row its own text line').toContain(
      'its logins and cookies go with it',
    );
    expect(
      list(shown.target)?.querySelector('.bz-del.confirm')?.textContent?.trim(),
      'and the confirm is the pair delete it / keep',
    ).toBe('delete it');
    expect(shownText(shown.target), 'with keep beside it').toContain('keep');
    expect(shownText(shown.target), 'the row it is confirming for is still named').toContain(
      'hunt',
    );

    const keep = [...(list(shown.target)?.querySelectorAll('button') ?? [])].find(
      (button) => button.textContent?.trim() === 'keep',
    );
    click(keep);
    expect(shownText(shown.target), 'keep puts the verbs back').not.toContain(
      'its logins and cookies go with it',
    );
    expect(
      list(shown.target)?.querySelector('.bz-del.confirm'),
      'and nothing is left armed',
    ).toBeNull();
    shown.stop();
  });

  it('deletes after the confirm and says it is working while it waits', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const gate: { finish?: () => void } = {};
    vi.mocked(deleteProfile).mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          gate.finish = resolve;
        }),
    );
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    click(list(shown.target)?.querySelector('.bz-del'));
    click(list(shown.target)?.querySelector('.bz-del.confirm'));
    expect(deleteProfile, 'the confirm is what calls the shell').toHaveBeenCalledWith('hunt');
    expect(
      shownText(shown.target),
      'the row reports the work while the shell waits out whatever it waits',
    ).toContain('deleting...');

    // The delete answered: the outcome names what went, and the row itself
    // falls away with the files.
    vi.mocked(listProfiles).mockResolvedValue([]);
    gate.finish?.();
    await vi.waitFor(() => {
      expect(shownText(shown.target), 'the outcome says what went').toContain(
        "deleted the 'hunt' profile",
      );
    });
    expect(
      list(shown.target)?.querySelector('.bz-del'),
      'and the row itself falls away with the files',
    ).toBeNull();
    shown.stop();
  });

  it('keeps the panel open under a pointer inside it and lets the delete land', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    /**
     * **A real pointer's press, which the plain `.click()` helper never
     * sends.** The panel is PORTALED to the body, and the leave guard used
     * to close it on any pointerdown outside the SEGMENT - so live, every
     * one of its controls unmounted under the press while these tests
     * stayed green.
     */
    const press = (el: Element | null | undefined): void => {
      if (!(el instanceof HTMLElement)) throw new Error('nothing to press');
      el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }));
      flushSync();
    };

    press(list(shown.target)?.querySelector('.bz-del'));
    expect(list(shown.target), 'the panel stays under the press').not.toBeNull();
    click(list(shown.target)?.querySelector('.bz-del'));
    expect(shownText(shown.target), 'and the confirm is armed').toContain(
      'its logins and cookies go with it',
    );
    press(list(shown.target)?.querySelector('.bz-del.confirm'));
    click(list(shown.target)?.querySelector('.bz-del.confirm'));
    expect(deleteProfile, 'the delete ran').toHaveBeenCalledWith('hunt');
    shown.stop();
  });

  it('draws the delete outcome and stands the empty line aside for it', async () => {
    // The shell answers at once: an earlier test left a gated implementation
    // in place, and `mockClear` in `afterEach` clears calls, not those.
    vi.mocked(deleteProfile).mockImplementation(() => Promise.resolve(undefined));
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    click(list(shown.target)?.querySelector('.bz-del'));
    click(list(shown.target)?.querySelector('.bz-del.confirm'));
    vi.mocked(listProfiles).mockResolvedValue([]);
    await vi.waitFor(() => {
      expect(shownText(shown.target), 'the outcome names what went').toContain(
        "deleted the 'hunt' profile",
      );
    });
    expect(
      shownText(shown.target),
      'and the empty line stands aside - the outcome explains the emptiness',
    ).not.toContain('no named profiles yet');
    shown.stop();
  });

  it('re-reads when the host says it swept', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('1 profile'));

    // A seat despawned in the core and the shell swept; the notice is the
    // only word the page gets, and the resting count must follow it.
    vi.mocked(listProfiles).mockResolvedValue([]);
    shown.swept();
    await vi.waitFor(() => {
      expect(shown.target.querySelector('.bz-tog .n')?.textContent, 'the count followed').toBe(
        '0 profiles',
      );
    });
    shown.stop();
  });

  it('keeps the row and says why when the delete is refused', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    vi.mocked(deleteProfile).mockRejectedValueOnce('no profile named hunt on this client');
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    click(list(shown.target)?.querySelector('.bz-del'));
    click(list(shown.target)?.querySelector('.bz-del.confirm'));
    await vi.waitFor(() => {
      expect(shownText(shown.target)).toContain('no profile named hunt on this client');
    });
    expect(shownText(shown.target), 'a refused delete leaves the row standing').toContain('hunt');
    shown.stop();
  });

  it("keeps each row's deleting mark while another row's delete is still running", async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
      { name: 'scout', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const gates: Record<string, () => void> = {};
    vi.mocked(deleteProfile).mockImplementation(
      (name: string) =>
        new Promise<void>((resolve) => {
          gates[name] = resolve;
        }),
    );
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('scout'));

    const rowFor = (name: string) => {
      const row = [...(list(shown.target)?.querySelectorAll('.bz-it') ?? [])].find((it) =>
        it.textContent?.includes(name),
      );
      if (!(row instanceof HTMLElement)) throw new Error(`no row for ${name}`);
      return row;
    };
    const marks = () => (shownText(shown.target)?.match(/deleting\.\.\./g) ?? []).length;

    click(rowFor('hunt').querySelector('.bz-del'));
    click(rowFor('hunt').querySelector('.bz-del.confirm'));
    click(rowFor('scout').querySelector('.bz-del'));
    click(rowFor('scout').querySelector('.bz-del.confirm'));
    await vi.waitFor(() => {
      expect(marks(), 'both rows report the work at once').toBe(2);
    });

    // The first delete answers: its ROW falls away with the re-read - the
    // outcome line that names it stays, which is the status the click is
    // owed - and the second row's mark must survive the first's completion.
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'scout', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    gates.hunt?.();
    await vi.waitFor(() => {
      expect(
        [...(list(shown.target)?.querySelectorAll('.bz-it') ?? [])].some(
          (it) => it.querySelector('.nm')?.textContent === 'hunt',
        ),
        'the first row falls away',
      ).toBe(false);
      expect(shownText(shown.target), 'and its outcome names what went').toContain(
        "deleted the 'hunt' profile",
      );
      expect(marks(), "and the second row's mark is still there").toBe(1);
    });

    vi.mocked(listProfiles).mockResolvedValue([]);
    gates.scout?.();
    await vi.waitFor(() => {
      expect(marks(), 'the last completion clears it').toBe(0);
    });
    shown.stop();
  });

  it('lists a closed profile with no close and a way to delete it', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: '', running: false, windowed: false },
    ]);
    const shown = show(false, true, { org: 'Busytools', project: 'forge', label: 'lead' });
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    expect(shownText(shown.target), 'a closed profile says what it still holds').toContain(
      'its logins are still here',
    );
    expect(
      list(shown.target)?.querySelector('.bz-close'),
      'nothing is open for close to close',
    ).toBeNull();
    expect(
      list(shown.target)?.querySelector('.bz-del'),
      'and the delete is the door that reaches it',
    ).not.toBeNull();
    shown.stop();
  });

  it('takes escape as leaving the confirm, not the panel', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    click(list(shown.target)?.querySelector('.bz-del'));
    const confirm = list(shown.target)?.querySelector('.bz-del.confirm');
    if (!(confirm instanceof HTMLElement)) throw new Error('no confirm');
    confirm.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(shownText(shown.target), 'the warning goes').not.toContain(
      'its logins and cookies go with it',
    );
    expect(list(shown.target), 'and the list the reader was in stays open').not.toBeNull();
    shown.stop();
  });

  it('moves the focus onto keep when a delete arms, and escape backs out from there', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true, windowed: false },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shownText(shown.target)).toContain('hunt'));

    click(list(shown.target)?.querySelector('.bz-del'));
    await vi.waitFor(() => {
      expect(
        document.activeElement?.textContent?.trim(),
        'arming puts the focus on keep, where Escape reaches',
      ).toBe('keep');
    });

    const keep = list(shown.target)?.querySelector<HTMLButtonElement>('[data-keep="hunt"]') ?? null;
    if (keep === null) throw new Error('no keep');
    keep.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(shownText(shown.target), 'the warning goes').not.toContain(
      'its logins and cookies go with it',
    );
    expect(list(shown.target), 'and the list stays open').not.toBeNull();
    expect(document.activeElement, 'with the focus back on the toggle').toBe(toggle(shown.target));
    shown.stop();
  });

  it('opens on hover and closes once the pointer leaves', async () => {
    const shown = show(false, true);
    expect(list(shown.target), 'closed at rest').toBeNull();

    toggle(shown.target).dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    expect(list(shown.target), 'the pointer arriving opens it').not.toBeNull();

    toggle(shown.target).dispatchEvent(pointer('pointerleave', 'mouse'));
    await vi.waitFor(() => {
      expect(list(shown.target), 'and leaving closes it').toBeNull();
    });
    shown.stop();
  });

  it('reads a tap as open: the synthesised enter must not eat the click', () => {
    const shown = show(false, true);
    // A finger: pointerenter arrives with pointerType touch, then the click.
    toggle(shown.target).dispatchEvent(pointer('pointerenter', 'touch'));
    toggle(shown.target).click();
    flushSync();

    expect(list(shown.target), 'the first tap leaves the panel open').not.toBeNull();
    shown.stop();
  });

  it('reads a tap as open when the press itself focuses the toggle first', () => {
    // Chromium's measured tap order: pointerdown and pointerup complete
    // first, then the compat mousedown, the focus it causes, and the click.
    // The focus a press puts there must not open the panel for the click to
    // shut - an open the reader never saw.
    const shown = show(false, true);
    window.dispatchEvent(pointer('pointerdown', 'touch'));
    window.dispatchEvent(pointer('pointerup', 'touch'));
    toggle(shown.target).dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    toggle(shown.target).dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    toggle(shown.target).dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
    toggle(shown.target).click();
    flushSync();

    expect(list(shown.target), 'the tap opens and stays open').not.toBeNull();
    shown.stop();
  });

  it('keeps the panel while the pointer crosses into it', async () => {
    const shown = show(false, true);
    toggle(shown.target).dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    toggle(shown.target).dispatchEvent(pointer('pointerleave', 'mouse'));
    const panel = list(shown.target);
    if (!(panel instanceof HTMLElement)) throw new Error('no panel');
    panel.dispatchEvent(pointer('pointerenter', 'mouse'));

    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(list(shown.target), 'the crossing is not a departure').not.toBeNull();
    shown.stop();
  });

  it('opens on a keyboard focus', () => {
    const shown = show(false, true);
    toggle(shown.target).dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();

    expect(list(shown.target), 'focus opens it').not.toBeNull();
    shown.stop();
  });

  it('opens on a keyboard focus after a press that focused nothing', () => {
    // A press on the already-focused toggle: no focusin consumes the press,
    // so the mouseup is what lets go - without it the next Tab would find
    // the door shut.
    const shown = show(false, true);
    toggle(shown.target).dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    toggle(shown.target).dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
    flushSync();
    toggle(shown.target).dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();

    expect(list(shown.target), 'the keyboard focus still opens it').not.toBeNull();
    shown.stop();
  });

  it('keeps the panel while focus crosses into it, and drops it when focus leaves', async () => {
    const shown = show(false, true);
    click(toggle(shown.target));
    const control = list(shown.target)?.querySelector<HTMLButtonElement>('.bz-override') ?? null;
    if (control === null) throw new Error('no control');

    // Tabbing from the toggle into a control: a bubbling focusout whose
    // related target is inside the segment is a crossing, not a leave.
    control.focus();
    toggle(shown.target).dispatchEvent(
      new FocusEvent('focusout', { bubbles: true, relatedTarget: control }),
    );
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(list(shown.target), 'the crossing keeps it').not.toBeNull();

    toggle(shown.target).dispatchEvent(
      new FocusEvent('focusout', { bubbles: true, relatedTarget: document.body }),
    );
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(list(shown.target), 'and a real leave drops it').toBeNull();
    shown.stop();
  });

  it('collapses on the toggle, like every sibling row', () => {
    const shown = show(false, true);
    click(toggle(shown.target));
    expect(list(shown.target), 'opened').not.toBeNull();

    click(toggle(shown.target));
    expect(list(shown.target), 'and the same click closes what the reader saw').toBeNull();
    shown.stop();
  });

  it('keeps the panel while a control inside it holds focus', async () => {
    const shown = show(false, true);
    toggle(shown.target).dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    const control = list(shown.target)?.querySelector<HTMLButtonElement>('.bz-override') ?? null;
    if (control === null) throw new Error('no control');
    control.focus();

    toggle(shown.target).dispatchEvent(pointer('pointerleave', 'mouse'));
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(list(shown.target), 'a leave does not unmount the focused control').not.toBeNull();
    shown.stop();
  });

  it('leaves focus on the toggle when Escape closes', () => {
    const shown = show(false, true);
    toggle(shown.target).dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    const control = list(shown.target)?.querySelector<HTMLButtonElement>('.bz-override') ?? null;
    if (control === null) throw new Error('no control');
    control.focus();

    control.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(list(shown.target), 'Escape closes the panel').toBeNull();
    expect(document.activeElement, 'and the toggle takes the focus').toBe(toggle(shown.target));
    shown.stop();
  });

  it('offers the override only where a click can honestly serve it, and sends it', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));

    const take = list(shown.target)?.querySelector('.bz-override');
    expect(take, 'capable and not hosting: the override is the door').not.toBeNull();
    click(take);
    expect(shown.taken, 'and pressing it asks the server for the role').toHaveBeenCalledTimes(1);
    shown.stop();
  });

  it('draws no override where this client cannot drive anything', () => {
    const shown = show(false, false);
    click(shown.target.querySelector('.bz-tog'));

    expect(shownText(shown.target)).toContain('browser not connected');
    expect(
      list(shown.target)?.querySelector('.bz-override'),
      'nothing to override with no host',
    ).toBeNull();
    shown.stop();
  });

  it('flips on the role frame: holding it drops the override and says so', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    expect(list(shown.target)?.querySelector('.bz-override')).not.toBeNull();

    shown.flip(true);
    flushSync();

    expect(shownText(shown.target), 'the role frame is what the line reads').toContain(
      'browser connected',
    );
    expect(
      list(shown.target)?.querySelector('.bz-override'),
      'a holder offers no override',
    ).toBeNull();
    shown.stop();
  });

  /** The show/hide button on the row whose `.nm` reads `name` (shared included). */
  function rowButton(target: HTMLElement, name: string): Element | null {
    // The rows live in the PORTALED panel, not under the mount.
    const panel = list(target);
    if (panel === null) return null;
    return (
      [...panel.querySelectorAll('.bz-it')]
        .find((el) => el.querySelector('.nm')?.textContent === name)
        ?.querySelector('button.bz-show') ?? null
    );
  }

  /** **The list is this slot's own**: another slot's profile is not this
   *  seat's business, and the count is the slot's, not the client's. */
  it('reads for its own slot and draws what the shell serves', async () => {
    // **The shell filters, the page does not filter again.** The read must
    // carry this page's seat - that is what makes another seat's profile
    // unreachable here - and the rows drawn are the ones the shell served.
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'mine', owner: 'Org/proj/me', running: true, windowed: false },
    ]);
    const shown = show(false, true, { org: 'Org', project: 'proj', label: 'me' });
    await vi.waitFor(() => {
      expect(listProfiles, 'the read carries this seat').toHaveBeenCalledWith({
        org: 'Org',
        project: 'proj',
        label: 'me',
      });
    });
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .n')?.textContent,
        'the count is what the shell served',
      ).toBe('1 profile');
    });
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => {
      expect(shownText(shown.target), "this slot's profile is listed").toContain('mine');
    });
    shown.stop();
  });

  /** **A window that is up reads hide, on the shared line too**: the person
   *  lowers it from the strip rather than hunting the window's X. */
  it("the shared line's button lowers the window it raised", async () => {
    vi.mocked(profileWindowed).mockResolvedValue(true);
    const shown = show();
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => {
      expect(rowButton(shown.target, 'shared')?.textContent, 'a window up reads hide').toContain(
        'hide',
      );
    });
    click(rowButton(shown.target, 'shared'));
    expect(hideBrowser, 'the shared line lowers the shared window').toHaveBeenCalledWith(null);
    shown.stop();
  });

  /** A profile row toggles the same way, and **a refused hide is drawn** -
   *  the button said hide, so a silence would read as done. */
  it('a profile whose window is up says hide, and a refused hide is drawn', async () => {
    vi.mocked(listProfiles).mockResolvedValue([
      { name: 'hunt', owner: 'Org/proj/me', running: true, windowed: true },
    ]);
    vi.mocked(hideBrowser).mockResolvedValueOnce('no browser profile is open under hunt');
    const shown = show(false, true, { org: 'Org', project: 'proj', label: 'me' });
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => {
      expect(rowButton(shown.target, 'hunt')?.textContent, 'a window up reads hide').toContain(
        'hide',
      );
    });
    click(rowButton(shown.target, 'hunt'));
    expect(hideBrowser, 'the row lowers its own window').toHaveBeenCalledWith('hunt');
    await vi.waitFor(() => {
      expect(shownText(shown.target), 'a refused hide is drawn, not swallowed').toContain(
        'no browser profile is open under hunt',
      );
    });
    shown.stop();
  });

  /** **A failed shared-line read is drawn**: the catch that says so was
   *  revertible to a silence with every test green, and a button quietly
   *  pinned to "show" reads as a window that is down. */
  it('draws a shared-line read that failed', async () => {
    vi.mocked(profileWindowed).mockRejectedValueOnce('the browser window state would not read');
    const shown = show();
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => {
      expect(shownText(shown.target), 'a shared-line read that failed says so').toContain(
        'the browser window state would not read',
      );
    });
    shown.stop();
  });
});
