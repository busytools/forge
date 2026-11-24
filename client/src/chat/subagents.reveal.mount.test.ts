// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Leaves from './Leaves.svelte';
import { reveal } from './subagents.svelte';
import { fold, type Unit, type WorkRow } from './units';

/** One Bash call, as a turn carries it. */
const bash = (id: string): unknown => ({
  type: 'assistant',
  uuid: 'a1',
  message: {
    id: 'm-a1',
    role: 'assistant',
    model: 'deepseek-v4.1-flash',
    content: [{ type: 'tool_use', id, name: 'Bash', input: { command: 'just check' } }],
  },
});

const rowsOf = (units: Unit[]): WorkRow[] =>
  units.find((unit): unit is Extract<Unit, { kind: 'leaves' }> => unit.kind === 'leaves')?.rows ??
  [];

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = '';
});

/**
 * **The row answers by its raw tool_use id, not by the list key.**
 *
 * A process cell's reveal is handed the id the wire gave the call, while the
 * row's list key names the OCCURRENCE (it carries the frame and block a
 * repeated id arrived in). A selector built from the list key stopped
 * matching the moment a repeated id made the key occurrence-unique, and the
 * reveal failed silently. This mounts a real row so the query is answered by
 * the DOM rather than by a stub's spelling.
 */
describe('revealing a plain call row, mounted', () => {
  it('finds and opens the row by the id the wire gave the call', () => {
    // jsdom has no layout, so the scroll the reveal makes is stubbed.
    const proto = HTMLElement.prototype as unknown as { scrollIntoView?: () => void };
    const before = proto.scrollIntoView;
    proto.scrollIntoView = () => {};
    // The flash's removal timer is six seconds; fake time keeps the file quick.
    vi.useFakeTimers();
    const app = mount(Leaves, {
      target: document.body,
      props: { rows: rowsOf(fold([bash('call_0_x')])) },
    });
    try {
      flushSync();
      const row = document.querySelector<HTMLDetailsElement>('details.leaf');
      expect(row, 'the call row drew').not.toBeNull();
      expect(row?.getAttribute('data-callid'), 'and carries the raw wire id').toBe('call_0_x');

      expect(reveal('call_0_x'), 'the reveal finds it by that id').toBe(true);
      expect(row?.open, 'and opens it').toBe(true);
    } finally {
      void unmount(app);
      if (before === undefined) delete proto.scrollIntoView;
      else proto.scrollIntoView = before;
    }
  });
});
