// @vitest-environment jsdom
/**
 * The code panels colour when the reader can see them, not when they mount.
 *
 * A heavy turn is one row to the list, so every panel in it is in the DOM at
 * once; tokenising them there held the frame for most of a second (#2556).
 * The plain pass draws synchronously, which is what a panel nobody has
 * scrolled to stays - and a panel that scrolls in must still colour.
 *
 * **The highlighter is mocked**, so the pins are calls rather than timing: a
 * synchronous DOM check cannot tell a paint that has not started from one
 * that starts and resolves later, and the regression this guards is exactly
 * a paint that starts at mount.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Code from './Code.svelte';
import DiffCode from './DiffCode.svelte';

const highlighter = vi.hoisted(() => ({
  body: vi.fn(() => Promise.resolve('<span style="color:#abc">coloured</span>')),
  lines: vi.fn(() => Promise.resolve([[{ content: 'same', color: '#abc' }]])),
}));

vi.mock('../format/highlight', () => ({
  highlightedBody: highlighter.body,
  tokenisedLines: highlighter.lines,
}));

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

/** Let the mocked dynamic import and its promise chain run. */
const settled = async (): Promise<void> => {
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
};

afterEach(() => {
  document.body.innerHTML = '';
  highlighter.body.mockClear();
  highlighter.lines.mockClear();
});

describe('a code panel the reader has not reached', () => {
  it('paints nothing at mount, nothing out of view, once when seen, and once only', async () => {
    const app = mount(Code, {
      target: document.body,
      props: { language: 'just', text: 'set shell := ["bash", "-cu"]' },
    });
    try {
      flushSync();
      const drawn = document.querySelector<HTMLElement>('.code');
      if (drawn === null) throw new Error('the panel drew nothing');
      await settled();
      expect(highlighter.body, 'nothing is tokenised at mount').not.toHaveBeenCalled();
      expect(drawn.querySelector('pre span[style*="color"]'), 'the plain pass draws').toBeNull();

      const watcher = watching(drawn);
      watcher.fire(drawn, false);
      await settled();
      expect(highlighter.body, 'nothing is tokenised out of view').not.toHaveBeenCalled();

      watcher.fire(drawn, true);
      await vi.waitFor(() => expect(highlighter.body).toHaveBeenCalledTimes(1), {
        timeout: 10_000,
      });
      await settled();
      expect(
        drawn.querySelector('pre span[style*="color"]'),
        'it colours when seen',
      ).not.toBeNull();

      watcher.fire(drawn, true);
      await settled();
      expect(highlighter.body, 'the first entry is the only paint').toHaveBeenCalledTimes(1);
    } finally {
      void unmount(app);
    }
  });

  it('unregisters a panel that unmounts before it is seen', async () => {
    const app = mount(Code, {
      target: document.body,
      props: { language: 'just', text: 'set shell := ["bash", "-cu"]' },
    });
    flushSync();
    const drawn = document.querySelector<HTMLElement>('.code');
    if (drawn === null) throw new Error('the panel drew nothing');
    const watcher = watching(drawn);
    void unmount(app);
    await settled();

    expect(watcher.targets.has(drawn), 'the observer let the panel go').toBe(false);
    watcher.fire(drawn, true);
    await settled();
    expect(highlighter.body, 'a panel that left never paints').not.toHaveBeenCalled();
  });
});

describe('a diff panel the reader has not reached', () => {
  it('tokenises only when the hunk scrolls in', async () => {
    const app = mount(DiffCode, {
      target: document.body,
      props: {
        path: 'src/example.ts',
        pieces: [
          {
            kind: 'hunk',
            header: '@@ -1,2 +1,2 @@',
            lines: [
              { kind: 'ctx', text: 'same', old: 1, new: 1 },
              { kind: 'add', text: 'added', old: null, new: 2 },
            ],
          },
        ],
      },
    });
    try {
      flushSync();
      const drawn = document.querySelector<HTMLElement>('.code');
      if (drawn === null) throw new Error('the panel drew nothing');
      await settled();
      expect(highlighter.lines, 'a hunk nobody sees is not tokenised').not.toHaveBeenCalled();

      watching(drawn).fire(drawn, true);
      await vi.waitFor(() => expect(highlighter.lines).toHaveBeenCalled(), { timeout: 10_000 });
      await settled();
      expect(
        highlighter.lines.mock.calls.length,
        'both sides of the hunk are tokenised once seen',
      ).toBeGreaterThan(0);
    } finally {
      void unmount(app);
    }
  });
});
