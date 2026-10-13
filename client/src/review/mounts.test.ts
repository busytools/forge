// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { Mounts } from './mounts';

/**
 * The roots a hand-built row owns.
 *
 * The canvas builds its rows as DOM and mounts Svelte pieces into them, so
 * nothing Svelte-side tears a root down when its row leaves. The sweep is the
 * half that covers the removals the reconciler never sees - a folded file, a
 * scope or a mode switch, the canvas going away - and each case below has a
 * row that must NOT be disturbed, so a sweep that releases everything fails
 * as loudly as one that releases nothing.
 */
describe('the roots a hand-built row owns', () => {
  it('releases a dropped row and only that row', () => {
    const mounts = new Mounts();
    const row = document.createElement('tr');
    const host = document.createElement('div');
    row.append(host);
    document.body.append(row);
    const outside = document.createElement('div');
    document.body.append(outside);

    let dropped = 0;
    let kept = 0;
    mounts.hold(host, () => (dropped += 1));
    mounts.hold(outside, () => (kept += 1));

    mounts.release(row);
    expect(dropped, "the row's own root went").toBe(1);
    expect(kept, 'and a root outside the row stayed').toBe(0);
  });

  it('sweeps a root whose element has left the document, and only those', () => {
    const mounts = new Mounts();
    const gone = document.createElement('div');
    const here = document.createElement('div');
    document.body.append(gone, here);
    let released = 0;
    let standing = 0;
    mounts.hold(gone, () => (released += 1));
    mounts.hold(here, () => (standing += 1));

    gone.remove();
    mounts.sweep();

    expect(released, 'a root whose row left the document is let go').toBe(1);
    expect(standing, 'a row still on the page keeps its root').toBe(0);
    mounts.sweep();
    expect(released, 'and a disposer never runs twice').toBe(1);
  });

  it('releases everything a page that is going away holds', () => {
    const mounts = new Mounts();
    const one = document.createElement('div');
    const two = document.createElement('div');
    document.body.append(one, two);
    let released = 0;
    mounts.hold(one, () => (released += 1));
    mounts.hold(two, () => (released += 1));

    mounts.releaseAll();
    expect(released, 'both, connected or not').toBe(2);
    mounts.sweep();
    expect(released, 'and nothing is left for a later sweep').toBe(2);
  });
});
