// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';

import { guardBackspaceNavigation } from './backspace';

/** One keydown, as WebKit hands it to the page. */
function press(target: EventTarget, init: KeyboardEventInit = {}): KeyboardEvent {
  const event = new KeyboardEvent('keydown', {
    key: 'Backspace',
    bubbles: true,
    cancelable: true,
    ...init,
  });
  target.dispatchEvent(event);
  return event;
}

let release: (() => void) | null = null;

afterEach(() => {
  release?.();
  release = null;
});

/**
 * **A bare Backspace is a browser-back on macOS WebKit.** `BackspaceKeyNavigationEnabled`
 * defaults on there, so a keydown with no editor under it walks the webview's
 * own history - and the client is an SPA whose history entries are the
 * router's. A take replaces the field it was typed in with its card, so the
 * next Backspace has no editor to stay in: it moved the reader a whole
 * session back (live, 2026-11-05).
 */
describe('a stray Backspace', () => {
  it('cannot move the client, because the webview default is cancelled', () => {
    release = guardBackspaceNavigation();

    expect(press(document.body).defaultPrevented).toBe(true);
  });

  it("stays the editor's where a field has the keys", () => {
    release = guardBackspaceNavigation();
    const field = document.createElement('textarea');
    document.body.append(field);
    try {
      expect(press(field).defaultPrevented).toBe(false);
    } finally {
      field.remove();
    }
  });
});
