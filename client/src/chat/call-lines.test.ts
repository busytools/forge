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
  it('draws the mutation size line under its diff, figures and marks as one run', () => {
    // **Mounted rather than rendered to a string**: the marks and the figures
    // are what the row's own state decides, and the line sits in the same row
    // as them rather than in a box of its own.
    app = mount(Call, { target: document.body, props: { call: edited() } });
    flushSync();

    const line = document.querySelector('.patchline');
    expect(line, 'the row drew no size line').not.toBeNull();
    expect(line?.textContent ?? '', 'the figures and both marks read as one run').toContain(
      '1 hunk \u{b7} +1 \u{2212}1',
    );
  });
});
