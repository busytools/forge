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
});
