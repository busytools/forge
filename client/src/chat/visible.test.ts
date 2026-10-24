// @vitest-environment jsdom
/**
 * The code panels colour when the reader can see them, not when they mount.
 *
 * A heavy turn is one row to the list, so every panel in it is in the DOM at
 * once; tokenising them there held the frame for most of a second. The plain
 * pass draws synchronously, which is what a panel nobody has scrolled to
 * stays - and a panel that scrolls in must still colour.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Code from './Code.svelte';

/** The observer the page shares, as the test drives it by hand. */
interface Watcher {
  targets: Set<Element>;
  fire: (node: Element, seen: boolean) => void;
}

const watchers: Watcher[] = [];

class FakeObserver {
  readonly targets = new Set<Element>();
  private readonly callback: IntersectionObserverCallback;

  constructor(callback: IntersectionObserverCallback) {
    this.callback = callback;
    watchers.push(this);
  }

  observe(node: Element): void {
    this.targets.add(node);
  }

  unobserve(node: Element): void {
    this.targets.delete(node);
  }

  disconnect(): void {
    this.targets.clear();
  }

  takeRecords(): IntersectionObserverEntry[] {
    return [];
  }

  fire(node: Element, seen: boolean): void {
    this.callback(
      [{ target: node, isIntersecting: seen } as IntersectionObserverEntry],
      this as unknown as IntersectionObserver,
    );
  }
}

(globalThis as unknown as { IntersectionObserver: unknown }).IntersectionObserver = FakeObserver;

/** The shared observer watching `node`. */
function watching(node: Element): Watcher {
  const found = watchers.find((one) => one.targets.has(node));
  if (found === undefined) throw new Error('no observer is watching the panel');
  return found;
}

afterEach(() => {
  document.body.innerHTML = '';
});

describe('a code panel the reader has not reached', () => {
  it('stays plain out of view and colours when it scrolls in', async () => {
    const app = mount(Code, {
      target: document.body,
      props: { language: 'just', text: 'set shell := ["bash", "-cu"]\n\ncheck:\n  cargo fmt' },
    });
    try {
      flushSync();
      const drawn = document.querySelector<HTMLElement>('.code');
      if (drawn === null) throw new Error('the panel drew nothing');
      expect(drawn.querySelector('pre span[style*="color"]'), 'plain at first').toBeNull();

      watching(drawn).fire(drawn, false);
      flushSync();
      expect(drawn.querySelector('pre span[style*="color"]'), 'still plain out of view').toBeNull();

      watching(drawn).fire(drawn, true);
      await vi.waitFor(
        () => {
          flushSync();
          if (drawn.querySelector('pre span[style*="color"]') === null) {
            throw new Error('waiting for the highlighter');
          }
        },
        { timeout: 10_000 },
      );
      expect(drawn.innerHTML, 'the tokens carry the theme colours once seen').toMatch(/color:\s*#/);
    } finally {
      void unmount(app);
    }
  });
});
