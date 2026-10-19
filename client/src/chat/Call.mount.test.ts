// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Call from './Call.svelte';
import type { ToolLeaf } from './leaves';
import { outputs } from './outputs.svelte';
import Refold from './testing/call-refold.svelte';

/** The live tail's cadence, pinned here as its value so a change to it is a red test. */
const TICK_MS = 1000;

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
   * The on-open read is the row's own ask: a settled backgrounded call reads
   * once per open, and the ask goes over the column's connection, which the
   * callback is.
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
   * **A running row reads on a tick.** The file the CLI streams a backgrounded
   * call's output to is there from launch - its path is composed from the task
   * id the launch frame carries - so while the row is open and the call still
   * runs the ask repeats, and the box follows the command as it writes.
   *
   * The settle ends the tick, and asks once more for the tail's final state:
   * the last tick's answer is the file as it stood a moment before the end.
   */
  it('asks on a tick while the row is open and the call still runs', () => {
    vi.useFakeTimers();
    try {
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
        expect(asked, 'the open asks at once, with no wait for the first tick').toEqual([
          'toolu_bgwatch',
        ]);
        vi.advanceTimersByTime(TICK_MS * 3);
        expect(asked.length, 'and the tick keeps asking while the call runs').toBe(4);

        app.settle();
        flushSync();
        expect(asked.length, 'the settle asks once more, for the ending tail').toBe(5);
        vi.advanceTimersByTime(TICK_MS * 5);
        expect(asked.length, 'and the tick is over - a settled row reads once').toBe(5);
      } finally {
        void unmount(app);
      }
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * **A run in flight is the case the fold's re-handing would storm.** The
   * row receives a NEW object on every frame its streaming turn appends, and
   * the settle guard does not cover the running path - so a running row that
   * re-asked per fold would send an ask per frame and re-arm the tick's
   * phase with it. The tick is armed by the running transition alone.
   */
  it('asks nothing extra when the fold re-hands a running call', () => {
    vi.useFakeTimers();
    try {
      const asked: string[] = [];
      const app = mount(Refold, {
        target: document.body,
        props: {
          call: { ...backgrounded, status: 'in_progress' as const },
          onreadoutput: (id: string) => asked.push(id),
        },
      }) as unknown as { refold: () => void };
      try {
        flushSync();
        expect(asked.length, 'the open asks at once').toBe(1);
        app.refold();
        app.refold();
        flushSync();
        expect(asked.length, 'value-identical re-folds ask nothing').toBe(1);
        vi.advanceTimersByTime(TICK_MS);
        expect(asked.length, 'and the tick keeps its own phase').toBe(2);
      } finally {
        void unmount(app);
      }
    } finally {
      vi.useRealTimers();
    }
  });

  /** A running call the reader closes reads nothing: no tick outlives the open. */
  it('stops the tick when the row closes', () => {
    vi.useFakeTimers();
    try {
      const asked: string[] = [];
      const app = mount(Call, {
        target: document.body,
        props: {
          k: 'f6',
          call: { ...backgrounded, status: 'in_progress' as const },
          onreadoutput: (id: string) => asked.push(id),
        },
      });
      try {
        flushSync();
        const row = document.querySelector('details');
        if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
        row.open = true;
        row.dispatchEvent(new Event('toggle'));
        flushSync();
        expect(asked.length, 'the open asks at once').toBe(1);

        row.open = false;
        row.dispatchEvent(new Event('toggle'));
        flushSync();
        vi.advanceTimersByTime(TICK_MS * 5);
        expect(asked.length, 'a closed row ticks nothing').toBe(1);
      } finally {
        void unmount(app);
      }
    } finally {
      vi.useRealTimers();
    }
  });

  /** And an unmounted row takes its tick with it - no timer outlives the row. */
  it('stops the tick when the row unmounts', () => {
    vi.useFakeTimers();
    try {
      const asked: string[] = [];
      const app = mount(Call, {
        target: document.body,
        props: {
          k: 'f7',
          call: { ...backgrounded, status: 'in_progress' as const },
          open: true,
          onreadoutput: (id: string) => asked.push(id),
        },
      });
      flushSync();
      vi.advanceTimersByTime(TICK_MS);
      const askedWhileMounted = asked.length;
      expect(askedWhileMounted, 'the open ask and the first tick both landed').toBe(2);
      void unmount(app);
      vi.advanceTimersByTime(TICK_MS * 5);
      expect(asked.length, 'nothing ticks after the row is gone').toBe(askedWhileMounted);
    } finally {
      vi.useRealTimers();
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

  /**
   * A capped edit names what the page left out on its marks line, beside the
   * figures it already draws - not as a body line of its own (#2374).
   */
  it('draws an omitted edit on the marks line, not as body prose', () => {
    const capped: ToolLeaf = {
      ...backgrounded,
      id: 'toolu_edit',
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'a.rs',
      backgrounded: false,
      body: [{ kind: 'diff', old: 'one', new: 'two' }],
      mutation: {
        hunks: 1,
        added: 1,
        removed: 1,
        all: false,
        outside: true,
        omitted: 5_200_000,
      },
    };
    const app = mount(Call, {
      target: document.body,
      props: { k: 'f6', call: capped, open: true },
    });
    try {
      flushSync();
      expect(
        Array.from(document.querySelectorAll('.patchline .patchmark')).map((el) => el.textContent),
        'the marks line carries both figures',
      ).toEqual(['changed outside this edit', 'omitted, 5.0 MB']);
      expect(
        Array.from(document.querySelectorAll('.term, .prose')).some((el) =>
          el.textContent.includes('omitted'),
        ),
        'and no body text repeats it',
      ).toBe(false);
    } finally {
      void unmount(app);
    }
  });

  /**
   * A capped question's row keeps the question's own glyph and opens on the
   * size the page left out, drawn in the diff's marks line (#2374).
   */
  it('draws a capped question as a question row that opens on the omission', () => {
    const capped: ToolLeaf = {
      ...backgrounded,
      id: 'toolu_q',
      row: { kind: 'family', family: 'question' },
      name: 'AskUserQuestion',
      title: 'Which colour?',
      backgrounded: false,
      body: [{ kind: 'omitted', bytes: 100_166 }],
    };
    const app = mount(Call, {
      target: document.body,
      props: { k: 'f5', call: capped, open: true },
    });
    try {
      flushSync();
      expect(
        document.querySelector('summary use')?.getAttribute('href'),
        'the question glyph, not the tool glyph',
      ).toBe('#i-question');
      expect(
        document.querySelector('.body .patchline .patchmark')?.textContent,
        'the omission on the marks line',
      ).toBe('omitted, 98 KB');
    } finally {
      void unmount(app);
    }
  });
});
