// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Card from './Card.svelte';

/**
 * A door in the system band takes both keys.
 *
 * Enter is an anchor's own; Space is not - a browser scrolls on it - so the
 * card forwards Space to the click itself. What this pins is the forwarding:
 * a keydown of `' '` on the card fires a click, and one on a card with no
 * href does nothing.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function draw(href: string | null): HTMLElement {
  app = mount(Card, {
    target: document.body,
    props: { title: 'extensions', tone: 'ready' as const, value: '22 plugins', detail: 'x', href },
  });
  flushSync();
  const element = document.querySelector('.svc');
  if (!(element instanceof HTMLElement)) throw new Error('the card did not draw');
  return element;
}

describe('a band card that is a way in', () => {
  it('opens on Space as it does on Enter', () => {
    const card = draw('/extensions');
    let clicks = 0;
    card.addEventListener('click', (event) => {
      clicks += 1;
      // jsdom cannot navigate; the handler is here to keep the click local.
      event.preventDefault();
    });

    card.dispatchEvent(new KeyboardEvent('keydown', { key: ' ', bubbles: true }));
    flushSync();
    expect(clicks, 'Space activates the door').toBe(1);

    // Enter is the anchor's own key and is left alone.
    const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    card.dispatchEvent(enter);
    expect(enter.defaultPrevented, 'Enter is not intercepted').toBe(false);
  });

  it('leaves a card that opens nothing alone', () => {
    const card = draw(null);
    let clicks = 0;
    card.addEventListener('click', () => {
      clicks += 1;
    });
    const space = new KeyboardEvent('keydown', { key: ' ', bubbles: true, cancelable: true });
    card.dispatchEvent(space);
    expect(clicks).toBe(0);
    expect(space.defaultPrevented).toBe(false);
  });
});
