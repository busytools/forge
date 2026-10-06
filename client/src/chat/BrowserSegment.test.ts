// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it, vi } from 'vitest';

import type { Connection } from '../socket';
import BrowserSegment from './BrowserSegment.svelte';

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

describe('the browser segment', () => {
  it('rests as a count and opens onto the role and the contexts', () => {
    const shown = show();
    expect(shown.target.textContent, 'the resting row names the count').toContain('0 contexts');
    expect(shown.target.textContent, 'and nothing about the role until it is open').not.toContain(
      'drives the browser',
    );

    click(shown.target.querySelector('.bz-tog'));

    expect(shown.target.textContent, 'a capable client that is not hosting is told so').toContain(
      'another client drives the browser',
    );
    expect(shown.target.textContent, 'with no contexts yet said plainly').toContain(
      'no contexts yet',
    );
    shown.stop();
  });

  it('offers Take over only where a click can honestly serve it, and sends it', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));

    const take = shown.target.querySelector('.bz-take');
    expect(take, 'capable and not hosting: the take is the door').not.toBeNull();
    click(take);
    expect(shown.taken, 'and pressing it asks the server for the role').toHaveBeenCalledTimes(1);
    shown.stop();
  });

  it('draws no take where this client cannot drive anything', () => {
    const shown = show(false, false);
    click(shown.target.querySelector('.bz-tog'));

    expect(shown.target.textContent).toContain('this client cannot drive the browser');
    expect(shown.target.querySelector('.bz-take'), 'nothing to take with no host').toBeNull();
    shown.stop();
  });

  it('flips on the role frame: holding it drops the take and says so', () => {
    const shown = show(false, true);
    click(shown.target.querySelector('.bz-tog'));
    expect(shown.target.querySelector('.bz-take')).not.toBeNull();

    shown.flip(true);
    flushSync();

    expect(shown.target.textContent, 'the role frame is what the line reads').toContain(
      'this client drives the browser',
    );
    expect(shown.target.querySelector('.bz-take'), 'a holder offers no take').toBeNull();
    shown.stop();
  });
});
