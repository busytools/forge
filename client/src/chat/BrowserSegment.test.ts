// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it, vi } from 'vitest';

import type { Connection } from '../socket';
import { closeContext, listContexts } from '../browser/host';
import BrowserSegment from './BrowserSegment.svelte';

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
    listContexts: vi.fn(() => Promise.resolve([])),
    closeContext: vi.fn(() => Promise.resolve(undefined)),
  };
});

/**
 * The connection the segment reads its role from and takes it through: a
 * stand-in for the two frames the server sends (the grant, and the loss to a
 * force-take).
 */
function fakeConnection(hosting = false) {
  const hearers = new Set<(now: boolean) => void>();
  let role = hosting;
  const taken = vi.fn();
  const connection = {
    browserRole: () => role,
    onBrowserRole: (fn: (now: boolean) => void) => {
      hearers.add(fn);
      return () => hearers.delete(fn);
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
    taken,
  };
}

/** The segment, mounted with whatever role and capability the test names. */
function show(hosting = false, capable = true) {
  const harness = fakeConnection(hosting);
  const target = document.createElement('div');
  document.body.append(target);
  const app = mount(BrowserSegment, { target, props: { connection: harness.connection, capable } });
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
  return target.querySelector('.bz-list');
}

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe('the browser segment', () => {
  it('rests as a count and opens onto the role and the contexts', async () => {
    const shown = show();
    await vi.waitFor(() => {
      expect(
        shown.target.querySelector('.bz-tog .n')?.textContent,
        'the resting row names the count once a read has answered',
      ).toBe('0 contexts');
    });
    expect(shown.target.textContent, 'and nothing about the role until it is open').not.toContain(
      'drives the browser',
    );

    click(shown.target.querySelector('.bz-tog'));

    expect(shown.target.textContent, 'a capable client that is not hosting is told so').toContain(
      'another client drives the browser',
    );
    await vi.waitFor(() => {
      expect(
        shown.target.textContent,
        'with no contexts yet said plainly, once the read has answered',
      ).toContain('no contexts yet');
    });
    shown.stop();
  });

  it('waits on the count and the list until a read has answered', () => {
    // A read that has not answered: neither surface may claim a state.
    vi.mocked(listContexts).mockImplementation(() => new Promise<never>(() => undefined));
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));

    expect(
      shown.target.querySelector('.bz-tog .n')?.textContent,
      'the count is not known before a read answers',
    ).toBe('…');
    expect(shown.target.textContent, 'and the list says it is reading').toContain(
      'reading the contexts…',
    );
    expect(
      shown.target.textContent,
      'an empty row would be a claim about a read that never answered',
    ).not.toContain('no contexts yet');
    shown.stop();
    vi.mocked(listContexts).mockImplementation(() => Promise.resolve([]));
  });

  it('says the read failed rather than claiming there are no contexts', async () => {
    const shown = show(false, true);
    await vi.waitFor(() => expect(listContexts).toHaveBeenCalledTimes(1));
    vi.mocked(listContexts).mockRejectedValueOnce('the context list would not read');

    click(shown.target.querySelector('.bz-tog'));

    await vi.waitFor(() => {
      expect(shown.target.textContent).toContain('the context list would not read');
    });
    expect(
      shown.target.textContent,
      'an empty row would be a claim about a read that never answered',
    ).not.toContain('no contexts yet');
    shown.stop();
  });

  it('keeps the row and says why when the close is refused', async () => {
    vi.mocked(listContexts).mockResolvedValue([
      { name: 'hunt', owner: 'Busytools/forge/lead', running: true },
    ]);
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    await vi.waitFor(() => expect(shown.target.textContent).toContain('hunt'));

    vi.mocked(closeContext).mockRejectedValueOnce('no browser context is open under hunt');
    click(shown.target.querySelector('.bz-close'));

    await vi.waitFor(() => {
      expect(shown.target.textContent).toContain('no browser context is open under hunt');
    });
    expect(shown.target.textContent, 'the row is not taken away by a close that failed').toContain(
      'hunt',
    );
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
    const control = shown.target.querySelector<HTMLButtonElement>('.bz-show');
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
    const control = shown.target.querySelector<HTMLButtonElement>('.bz-show');
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
    const control = shown.target.querySelector<HTMLButtonElement>('.bz-show');
    if (control === null) throw new Error('no control');
    control.focus();

    control.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(list(shown.target), 'Escape closes the panel').toBeNull();
    expect(document.activeElement, 'and the toggle takes the focus').toBe(toggle(shown.target));
    shown.stop();
  });

  it('offers Take over only where a click can honestly serve it, and sends it', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));

    const take = shown.target.querySelector('.bz-takeover');
    expect(take, 'capable and not hosting: the take is the door').not.toBeNull();
    click(take);
    expect(shown.taken, 'and pressing it asks the server for the role').toHaveBeenCalledTimes(1);
    shown.stop();
  });

  it('draws no take where this client cannot drive anything', () => {
    const shown = show(false, false);
    click(shown.target.querySelector('.bz-tog'));

    expect(shown.target.textContent).toContain('this client cannot drive the browser');
    expect(shown.target.querySelector('.bz-takeover'), 'nothing to take with no host').toBeNull();
    shown.stop();
  });

  it('flips on the role frame: holding it drops the take and says so', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    expect(shown.target.querySelector('.bz-takeover')).not.toBeNull();

    shown.flip(true);
    flushSync();

    expect(shown.target.textContent, 'the role frame is what the line reads').toContain(
      'this client drives the browser',
    );
    expect(shown.target.querySelector('.bz-takeover'), 'a holder offers no take').toBeNull();
    shown.stop();
  });
});
