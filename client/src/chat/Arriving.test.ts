// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, describe, expect, it } from 'vitest';

import { leafOf, type ToolLeaf } from './leaves';
import Arriving from './testing/Arriving.svelte';

/** The call before its result, and the same call once the decision parsed. */
function pair(): { before: ToolLeaf; after: ToolLeaf } {
  const input = { state: 'a one-line import fix', instructions: 'Is this mechanical?' };
  const before = leafOf('tu-live', 'mcp__forge__systemone__ask_noul', input, undefined);
  const after = leafOf('tu-live', 'mcp__forge__systemone__ask_noul', input, {
    type: 'tool_result',
    content: '{"model":"jev-1.13.0","answer":{"type":"noul","noul":0.93}}',
  });
  return { before, after };
}

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('a decision that arrives after its row mounted', () => {
  it('opens on the result, and the reader owns the toggle after that', () => {
    const arrived = writable(false);
    const { before, after } = pair();
    app = mount(Arriving, { target: document.body, props: { before, after, arrived } });
    flushSync();

    const row = (): HTMLDetailsElement | null => document.querySelector('details.leaf');
    expect(row()?.open, 'the call in flight draws closed').toBe(false);
    expect(row()?.textContent ?? '', 'and holds nothing yet').not.toContain('0.93');

    arrived.set(true);
    flushSync();

    expect(
      row()?.open,
      'the result opens the row, which a mount-time snapshot alone cannot do',
    ).toBe(true);
    expect(document.body.textContent ?? '', 'and the block draws inside it').toContain('0.93');

    // The reader's own close wins from here. The row's effect reads its state
    // untracked precisely so this write does not re-run it - a tracked read
    // re-fires on the close itself and re-opens the row.
    row()?.querySelector('summary')?.click();
    flushSync();
    expect(row()?.open, 'and the reader close is not undone').toBe(false);
  });
});
