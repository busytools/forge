// @vitest-environment jsdom
import { beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('svelte', () => ({ mount: vi.fn() }));
vi.mock('./perf', () => ({ startPerfProbe: vi.fn() }));
vi.mock('./App.svelte', () => ({ default: class App {} }));

beforeAll(async () => {
  document.body.innerHTML = '<div id="app"></div>';
  await import('./main');
});

/**
 * **The entry installs the guard itself**, so a boot refactor cannot drop it
 * while every other test stays green.
 */
describe('the app entry', () => {
  it("installs the page's Backspace guard", () => {
    const event = new KeyboardEvent('keydown', {
      key: 'Backspace',
      bubbles: true,
      cancelable: true,
    });

    document.body.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
  });
});
