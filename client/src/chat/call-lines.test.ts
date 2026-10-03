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
  decision: null,
  skill: null,
  image: null,
  imageNote: null,
});

/** A failed edit: the two sides its own input carries, and the CLI's own reason under them. */
const refused = (): ToolLeaf => ({
  ...edited(),
  status: 'failed',
  body: [
    { kind: 'diff', old: 'a = 1;', new: 'a = 2;' },
    {
      kind: 'error',
      message: 'String to replace not found in file.',
      detail: 'String:   a = 3;',
    },
  ],
});

/** A failed command: what it ran, and the reason it came back with. */
const refusedBash = (): ToolLeaf => ({
  id: 'toolu_bash',
  row: { kind: 'family', family: 'bash' },
  name: 'Bash',
  title: 'run the gate against the wrong tree',
  command: 'just check --nope',
  status: 'failed',
  note: null,
  body: [{ kind: 'error', message: 'Command failed with exit code 2', detail: '' }],
  mutation: null,
  decision: null,
  skill: null,
  image: null,
  imageNote: null,
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
    app = mount(Call, { target: document.body, props: { k: 'toolu_edit', call: edited() } });
    flushSync();

    const line = document.querySelector('.patchline');
    expect(line, 'the row drew no size line').not.toBeNull();
    expect(line?.textContent ?? '', 'the figures and both marks read as one run').toContain(
      '1 hunk \u{b7} +1 \u{2212}1',
    );
  });

  it('draws a position-less diff without the number columns, and the reason under it', () => {
    app = mount(Call, { target: document.body, props: { k: 'toolu_edit', call: refused() } });
    flushSync();

    const dif = document.querySelector('.dif');
    expect(dif?.classList.contains('bare'), 'a diff with no position says so').toBe(true);
    expect(dif?.querySelectorAll('.on, .nn').length, 'and draws no number columns').toBe(0);

    // The reason draws in the hint chrome rather than as a box of its own:
    // the message first, its detail under it, the CLI's envelope read off in
    // the fold (Ved's shape, 2026-10-03).
    const hint = document.querySelector('.errhint');
    expect(hint, 'the reason draws in the hint chrome').not.toBeNull();
    expect(hint?.textContent, 'the message says what happened').toContain(
      'String to replace not found in file.',
    );
    expect(hint?.textContent, 'and the detail under it is not dropped').toContain(
      'String:   a = 3;',
    );
  });

  it('draws a failed command line above its reason, the way a settled one draws it', () => {
    // The command leads its own output on a settled row, and a failed one is
    // the same row: the reason under it says nothing about WHAT failed if the
    // line that names the command goes missing.
    app = mount(Call, { target: document.body, props: { k: 'toolu_bash', call: refusedBash() } });
    flushSync();

    const hint = document.querySelector('.errhint');
    expect(hint, 'the reason draws in the hint chrome').not.toBeNull();
    expect(hint?.textContent, 'the command that was run leads its own failure').toContain(
      'just check --nope',
    );
    expect(hint?.textContent, 'and the reason follows it').toContain(
      'Command failed with exit code 2',
    );
  });

  it('keeps the number columns on a diff that has a position', () => {
    app = mount(Call, { target: document.body, props: { k: 'toolu_edit', call: edited() } });
    flushSync();

    expect(
      document.querySelector('.dif')?.classList.contains('bare'),
      'a hunk has numbers to draw',
    ).toBe(false);
    expect(document.querySelectorAll('.dif .on').length, 'and the columns are drawn').toBe(2);
  });
});
