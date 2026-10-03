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
  skill: null,
  image: null,
  imageNote: null,
});

/** The body's term boxes, as the reader sees them. */
const boxes = (body: string): string[] =>
  [...body.matchAll(/<div class="term">([\s\S]*?)<\/div>/g)].map((box) => box[1] ?? '');

describe('the row one call draws', () => {
  /**
   * **The row carries the fold's own name, not the wire id.** Two id-less
   * `tool_use` calls leave the wire id empty, so the lane hands the fold's key
   * down and the row draws that: keys stay unique, which is what the column's
   * anchor would need of them.
   */
  it("carries the fold's key on the row", () => {
    const named = render(Call, { props: { call: backgrounded(null), k: 'f7' } }).body;
    expect(named, "the fold's own name for the row").toContain('data-k="call-f7"');
  });

  it("opens onto the skill a Skill call loaded, which is the row's right data", () => {
    // The call's own result is the CLI's "Launching skill: ..." line, which
    // says nothing; the fold hangs the skill's body on the call, and the row
    // opens onto that instead.
    const drawn = render(Call, {
      props: {
        k: 'toolu_skill',
        call: {
          id: 'toolu_skill',
          row: { kind: 'family', family: 'skill' },
          name: 'Skill',
          title: 'unslop',
          command: null,
          status: 'completed',
          note: null,
          body: [{ kind: 'text', text: 'Launching skill: unslop' }],
          mutation: null,
          skill: '# Unslop\n\nEdit text to remove AI patterns.',
          image: null,
          imageNote: null,
        } as ToolLeaf,
      },
    }).body;

    expect(drawn, 'the skill is drawn as markdown').toContain('<h1>');
    expect(drawn, 'and the launch line draws nowhere').not.toContain('Launching skill');
  });

  it('draws the picture a call read only while the row is open', () => {
    // Decoding a screenshot is real work, and a column of closed rows must
    // not pay it: the data URL reaches the markup only once the row opens.
    // The fold's test pins what the leaf carries; this pins what a closed row
    // refuses to draw.
    const drawn = render(Call, {
      props: {
        k: 'toolu_shot',
        call: {
          id: 'toolu_shot',
          row: { kind: 'family', family: 'read' },
          name: 'Read',
          title: '/Users/ved/shot.png',
          command: null,
          status: 'completed',
          note: null,
          body: [],
          mutation: null,
          skill: null,
          image: { mime: 'image/png', data: 'AAAA' },
          imageNote: 'original 100x100, displayed at 100x100.',
        } as ToolLeaf,
      },
    }).body;

    expect(drawn, 'the closed row carries no data URL').not.toContain('data:image');
    expect(drawn, 'and none of the caption either').not.toContain('Multiply');
  });

  it('draws the text a result carried beside the picture, not instead of it', () => {
    // A screenshot result carries the path it was saved to (and a PDF read its
    // provenance) as a text block of the same result. The picture drew from an
    // exclusive branch, so that text reached the page nowhere.
    const drawn = render(Call, {
      props: {
        open: true,
        k: 'toolu_shot_text',
        call: {
          id: 'toolu_shot_text',
          row: { kind: 'family', family: 'read' },
          name: 'Read',
          title: '/tmp/playwright/shot.png',
          command: null,
          status: 'completed',
          note: null,
          body: [
            { kind: 'text', text: 'Saved to /tmp/playwright/shot.png' },
            { kind: 'image', mime: 'image/png', uri: null },
          ],
          mutation: null,
          skill: null,
          image: { mime: 'image/png', data: 'AAAA' },
          imageNote: 'original 100x100, displayed at 100x100.',
        } as ToolLeaf,
      },
    }).body;

    expect(drawn, 'the picture draws').toContain('data:image');
    expect(drawn, 'and the text the same result carried draws with it').toContain(
      'Saved to /tmp/playwright/shot.png',
    );
  });

  it('draws a backgrounded call notice in the box its own result drew', () => {
    const drawn = boxes(
      render(Call, {
        props: {
          k: 'bg-notice',
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
          k: 'bg-two',
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
      props: { k: 'bg-running', call: { ...backgrounded(null), status: 'in_progress' } },
    }).body;

    expect(running, 'the row carries the running class').toContain('class="leaf running"');
    expect(
      render(Call, { props: { k: 'bg-clean', call: backgrounded(null) } }).body,
      'and a settled call does not',
    ).not.toContain('class="leaf running"');
  });
});
