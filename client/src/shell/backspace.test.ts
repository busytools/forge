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

/** A bare Backspace is a browser-back in macOS WebKit; a take leaves no editor to hold it. */
describe('a stray Backspace', () => {
  it("cancels the webview's Backspace default on the page", () => {
    release = guardBackspaceNavigation();

    expect(press(document.body).defaultPrevented).toBe(true);
  });

  it('leaves every other key alone', () => {
    release = guardBackspaceNavigation();

    expect(press(document.body, { key: 'Enter' }).defaultPrevented).toBe(false);
  });

  it("stays the editor's where a text field has the keys", () => {
    release = guardBackspaceNavigation();
    const textarea = document.createElement('textarea');
    const input = document.createElement('input');
    document.body.append(textarea, input);
    try {
      expect(press(textarea).defaultPrevented).toBe(false);
      expect(press(input).defaultPrevented).toBe(false);
    } finally {
      textarea.remove();
      input.remove();
    }
  });

  it('is cancelled on an input a Backspace cannot delete in', () => {
    release = guardBackspaceNavigation();
    const radio = document.createElement('input');
    radio.type = 'radio';
    document.body.append(radio);
    try {
      expect(press(radio).defaultPrevented).toBe(true);
    } finally {
      radio.remove();
    }
  });

  it('still runs when a bubble listener stops the event at the body', () => {
    release = guardBackspaceNavigation();
    const stopper = (event: KeyboardEvent): void => event.stopPropagation();
    document.body.addEventListener('keydown', stopper);
    const row = document.createElement('div');
    document.body.append(row);
    try {
      expect(press(row).defaultPrevented).toBe(true);
    } finally {
      document.body.removeEventListener('keydown', stopper);
      row.remove();
    }
  });

  it("stays the editor's on a contenteditable host", () => {
    release = guardBackspaceNavigation();
    const editable = document.createElement('div');
    editable.contentEditable = 'true';
    // jsdom implements no isContentEditable, so the host answers for itself.
    Object.defineProperty(editable, 'isContentEditable', { value: true });
    document.body.append(editable);
    try {
      expect(press(editable).defaultPrevented).toBe(false);
    } finally {
      editable.remove();
    }
  });
});
