// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Call from './Call.svelte';
import type { ToolLeaf } from './leaves';

/** An edit whose row draws a hunk and the one line under it. */
const edited = (): ToolLeaf => ({
  id: 'toolu_edit',
  row: { kind: 'family', family: 'edit' },
  name: 'Edit',
  title: '/x/a.css',
  command: null,
  status: 'completed',
  note: null,
  body: [
    {
      kind: 'hunk',
      header: '@@ -1,2 +1,2 @@',
      lines: [
        { kind: 'del', text: 'a = 1;', old: 1, new: null },
        { kind: 'add', text: 'a = 2;', old: null, new: 1 },
      ],
    },
  ],
  mutation: { hunks: 1, added: 1, removed: 1, all: true, outside: true },
});

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe("the lines a call's row draws", () => {
  it('keeps the mutation size line on one line, in the renderer that runs', () => {
    // **Mounted rather than rendered to a string**, because the two paths
    // disagree about whitespace and the client's is the one on screen: this box
    // keeps every newline it is given (`white-space: pre-wrap`, which exists so
    // a command's output keeps its own breaks), so a template line break inside
    // it stacks the figures - and the server renderer collapses the same break,
    // which is how a broken line reads as fixed.
    app = mount(Call, { target: document.body, props: { call: edited() } });
    flushSync();

    const boxes = document.querySelectorAll('.term');
    const line = boxes[boxes.length - 1]?.textContent ?? '';
    expect(line, 'the figures, the size and both marks read as one run').toContain(
      '1 hunk \u{b7} +1 \u{2212}1',
    );
    expect(line, 'and none of it carries a break').not.toContain('\n');
  });
});
