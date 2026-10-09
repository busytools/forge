// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Call from './Call.svelte';
import type { ToolLeaf } from './leaves';
import { outputs } from './outputs.svelte';
import Refold from './testing/call-refold.svelte';

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

  /**
   * The fold hands the row a NEW object on every frame the turn appends, so
   * an effect keyed on the call would ask per frame of the row's own
   * streaming turn - a dispatch, a file read and an answer each. The row
   * remembers the id it asked for in this open, so a re-fold asks nothing.
   */
  it('asks nothing more when the fold re-hands the call', () => {
    const asked: string[] = [];
    const app = mount(Refold, {
      target: document.body,
      props: { call: backgrounded, onreadoutput: (id: string) => asked.push(id) },
    }) as unknown as { refold: () => void };
    try {
      flushSync();
      app.refold();
      app.refold();
      flushSync();
      expect(asked, 'value-identical re-folds ask nothing').toEqual(['toolu_bgwatch']);
    } finally {
      void unmount(app);
    }
  });

  /**
   * A call still running has no file to read yet; the settle is the one
   * transition that turns a held row into a readable one, and it is what the
   * ask waits for.
   */
  it('asks when a row opened mid-run settles, and the run alone does not', () => {
    const asked: string[] = [];
    const app = mount(Refold, {
      target: document.body,
      props: {
        call: { ...backgrounded, status: 'in_progress' as const },
        onreadoutput: (id: string) => asked.push(id),
      },
    }) as unknown as { settle: () => void };
    try {
      flushSync();
      expect(asked, 'a running call has no file to read yet').toEqual([]);
      app.settle();
      flushSync();
      expect(asked, 'the settle is what makes it readable').toEqual(['toolu_bgwatch']);
    } finally {
      void unmount(app);
    }
  });

  /** Closing forgets the ask, so the re-open asks again - the read heals a file gone between visits. */
  it('asks again after a close and a re-open', () => {
    const asked: string[] = [];
    const app = mount(Call, {
      target: document.body,
      props: {
        k: 'f4',
        call: backgrounded,
        onreadoutput: (id: string) => asked.push(id),
      },
    });
    try {
      flushSync();
      expect(asked, 'a closed row asks nothing').toEqual([]);

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(asked, 'the open asks').toEqual(['toolu_bgwatch']);

      row.open = false;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(asked, 'and the re-open asks again').toEqual(['toolu_bgwatch', 'toolu_bgwatch']);
    } finally {
      void unmount(app);
    }
  });
});
