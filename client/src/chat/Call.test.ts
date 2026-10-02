import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Call from './Call.svelte';
import type { ToolLeaf } from './leaves';

/** A backgrounded call that has ended, as the fold hands it to the row. */
const backgrounded = (note: ToolLeaf['note']): ToolLeaf => ({
  id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
  row: { kind: 'family', family: 'bash' },
  name: 'Bash',
  title: 'Echo test string after brief sleep',
  command: 'sleep 2 && echo test string',
  status: 'completed',
  note,
  body: [{ kind: 'text', text: 'Command running in background with ID: bj5g0t2kq.' }],
  mutation: null,
});

/** The body's term boxes, as the reader sees them. */
const boxes = (body: string): string[] =>
  [...body.matchAll(/<div class="term">([\s\S]*?)<\/div>/g)].map((box) => box[1] ?? '');

describe('the row one call draws', () => {
  it('draws a backgrounded call notice in the box its own result drew', () => {
    const drawn = boxes(
      render(Call, {
        props: {
          call: backgrounded({
            text: 'Background command "Echo test string after brief sleep" completed (exit code 0)',
            tone: 'sum',
          }),
        },
      }).body,
    );

    expect(drawn, 'one box, as the drawing has it').toHaveLength(1);
    expect(drawn[0], 'the result first').toContain('Command running in background with ID');
    expect(drawn[0], 'and the notice as its last line').toContain('completed (exit code 0)');
    expect(drawn[0], 'in the tone the frame earned').toContain('class="sum"');
  });

  it('draws the notice once, in the last box, when the result is more than one line', () => {
    // A result carrying two text blocks draws two boxes, and the notice closes
    // the LAST of them: written into every box it would draw once per box.
    const call = backgrounded({
      text: 'Background command "Echo test string after brief sleep" completed (exit code 0)',
      tone: 'sum',
    });
    const drawn = boxes(
      render(Call, {
        props: {
          call: {
            ...call,
            body: [
              { kind: 'text', text: 'Command running in background with ID: bj5g0t2kq.' },
              { kind: 'text', text: 'and a second line of output' },
            ],
          },
        },
      }).body,
    );

    expect(drawn, 'two boxes, one per block').toHaveLength(2);
    expect(drawn[0], 'the notice is not in the first').not.toContain('exit code 0');
    expect(drawn[1], 'and closes the last').toContain('exit code 0');
  });

  it('says at the row that a call the wire reports as running is still out', () => {
    // The one thing a backgrounded call's own launch result cannot say: that
    // result is a clean one.
    const running = render(Call, {
      props: { call: { ...backgrounded(null), status: 'in_progress' } },
    }).body;

    expect(running, 'the row carries the running class').toContain('class="leaf running"');
    expect(
      render(Call, { props: { call: backgrounded(null) } }).body,
      'and a settled call does not',
    ).not.toContain('class="leaf running"');
  });
});
