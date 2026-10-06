// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { installResizeObserver } from '../chat/testing/viewport';
import ChatFixture from './ChatFixture.svelte';

// The chat's list is `virtua`'s, and jsdom has no ResizeObserver to offer it.
installResizeObserver();

/**
 * The canned page mounts the whole column, strip included, against the
 * connection this file's sibling answers method by method.
 *
 * **The pin is the mount.** A strip row that reads a connection method the
 * canned one does not answer throws at mount and takes the column down with
 * it - which is what happened when the browser row joined the strip while the
 * fixture had never been taught its three role methods. Asserting the strip
 * draws is asserting every method the current rows read is answered.
 */
let app: Record<string, unknown> | null = null;

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.useRealTimers();
});

describe('the canned page', () => {
  it('draws the strip against the canned connection', async () => {
    app = mount(ChatFixture, { target: document.body });
    // The canned page arrives through a dynamic import, so the dev bar is
    // the signal that the connection is live. `waitFor` advances the fake
    // clock while it waits, which is what lets a promise chain settle here.
    await vi.waitFor(() => {
      flushSync();
      expect(document.querySelector('.devbar'), 'the canned page never landed').not.toBeNull();
    });

    // One appended turn is what puts the pinned strip on screen: the strip
    // is drawn for a running turn.
    const append = [...document.querySelectorAll('button')].find(
      (button) => button.textContent === 'append a turn',
    );
    append?.click();
    await vi.waitFor(() => {
      flushSync();
      expect(document.querySelector('.strip'), 'the strip never drew').not.toBeNull();
    });

    expect(
      document.querySelector('.strip .sg-tog'),
      'a row read an unanswered method: no row drew',
    ).not.toBeNull();
  });
});
