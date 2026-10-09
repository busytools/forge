// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Call from './Call.svelte';
import type { ToolLeaf } from './leaves';
import { outputs } from './outputs.svelte';

/** A backgrounded call that has ended, as the fold hands it to the row. */
const backgrounded: ToolLeaf = {
  id: 'toolu_bgwatch',
  row: { kind: 'family', family: 'bash' },
  name: 'Bash',
  title: 'watch the run',
  command: 'gh run watch',
  status: 'completed',
  backgrounded: true,
  note: null,
  body: [{ kind: 'text', text: 'Command running in background with ID: bj5g0t2kq.' }],
  mutation: null,
  decision: null,
  forge: null,
  skill: null,
  image: null,
  imageNote: null,
};

describe('the call row, mounted', () => {
  afterEach(() => {
    outputs.clear('seat');
    document.body.innerHTML = '';
  });

  /**
   * The on-open read is the row's own ask, fired once per open: opening a
   * backgrounded call that has settled is the only moment the file it wrote
   * is worth reading, and the ask goes over the column's connection, which
   * the callback is.
   */
  it("asks for the call's own output when the reader opens it, once", () => {
    const asked: string[] = [];
    const app = mount(Call, {
      target: document.body,
      props: {
        k: 'f1',
        call: backgrounded,
        open: true,
        onreadoutput: (id: string) => asked.push(id),
      },
    });
    try {
      flushSync();
      flushSync();
      expect(asked, 'the open asks for this call, exactly once').toEqual(['toolu_bgwatch']);
    } finally {
      void unmount(app);
    }
  });

  /** A call that ran in the foreground has no task, so there is no file to ask about. */
  it('asks for nothing when the call ran in the foreground', () => {
    const asked: string[] = [];
    const app = mount(Call, {
      target: document.body,
      props: {
        k: 'f2',
        call: { ...backgrounded, id: 'toolu_fg', backgrounded: false },
        open: true,
        onreadoutput: (id: string) => asked.push(id),
      },
    });
    try {
      flushSync();
      expect(asked, 'no ask for a call with no task').toEqual([]);
    } finally {
      void unmount(app);
    }
  });

  /** A row with no column to ask through draws what it has and invents nothing. */
  it('asks for nothing when the row has no way to ask', () => {
    const app = mount(Call, {
      target: document.body,
      props: { k: 'f3', call: backgrounded, open: true },
    });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'the ack still draws').toContain('bj5g0t2kq');
    } finally {
      void unmount(app);
    }
  });
});
