import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Call from './Call.svelte';
import type { ToolLeaf } from './leaves';
import { subagents } from './subagents.svelte';
import type { SubagentCard } from '../session/wire';

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
  decision: null,
  forge: null,
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
   * `tool_use` calls leave the wire id empty, so the leaves list hands the
   * fold's key down and the row draws that: keys stay unique, which is what
   * the column's anchor would need of them.
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
          decision: null,
          forge: null,
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
          decision: null,
          forge: null,
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
          decision: null,
          forge: null,
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

  it('draws a parsed decision as its block, in place of the raw result', () => {
    // The result's JSON and the block carry the same facts; the row draws the
    // block. The fold's tests pin that an unreadable result keeps drawing its
    // text, which is this same branch not taken.
    const drawn = render(Call, {
      props: {
        k: 'toolu_decide',
        open: true,
        call: {
          id: 'toolu_decide',
          row: { kind: 'systemone' },
          name: 'mcp__forge__systemone__ask_noul',
          title: 'ask noul - Is this mechanical?',
          command: null,
          status: 'completed',
          note: null,
          body: [
            {
              kind: 'text',
              text: '{"model":"jev-1.13.0","answer":{"type":"noul","noul":0.93},"usage":{"input_tokens":392,"output_tokens":20}}',
            },
          ],
          mutation: null,
          decision: {
            model: 'jev-1.13.0',
            usage: { input_tokens: 392, output_tokens: 20, cost: null },
            answer: { kind: 'noul', noul: 0.93 },
            question: null,
            criteria: {},
          },
          forge: null,
          skill: null,
          image: null,
          imageNote: null,
        } as ToolLeaf,
      },
    }).body;

    expect(drawn, 'the block draws').toContain('class="dec"');
    expect(drawn, 'with the answer as its number').toContain('0.93');
    expect(drawn, 'and the raw result box no longer draws').not.toContain('class="term"');
  });
});

describe('the forge card row', () => {
  /** One `mcp__forge__tasks__update` call, as the fold hands it to the row. */
  const tasksUpdate = (over: Partial<ToolLeaf> = {}): ToolLeaf => ({
    id: 'toolu_tasks',
    row: { kind: 'forge', family: 'tasks' },
    name: 'mcp__forge__tasks__update',
    title: 'Wire the review notice into the chat',
    command: null,
    status: 'completed',
    note: null,
    body: [],
    mutation: null,
    decision: null,
    forge: {
      title: 'Wire the review notice into the chat',
      chips: [{ text: 'in progress', tone: 'info' }],
      figure: 'owner lead',
      pieces: [{ kind: 'kv', pairs: [['changed', 'status']] }],
    },
    skill: null,
    image: null,
    imageNote: null,
    ...over,
  });

  it('draws the subject as the title, the chip and figure beside it, the facts below', () => {
    const drawn = render(Call, { props: { call: tasksUpdate(), k: 't7' } }).body;

    // The subject, never the tool's name: `tasks__update` is plumbing.
    expect(drawn, 'the row is titled by the subject').toContain(
      'Wire the review notice into the chat',
    );
    expect(drawn, 'and never by the tool').not.toContain('tasks__update');
    expect(drawn, 'the state is a chip that carries its own word').toContain('fam-chip info');
    expect(drawn, 'with the word in it').toContain('in progress');
    expect(drawn, 'and the owner is the figure').toContain('owner lead');
    expect(drawn, 'the body draws the facts as one inline run').toContain('fam-meta');
    expect(drawn, 'with the changed field named').toContain('changed');
  });

  it('states a failed forge call reason on the row itself', () => {
    const drawn = render(Call, {
      props: {
        call: tasksUpdate({
          status: 'failed',
          forge: null,
          body: [{ kind: 'error', message: 'no task with id t-91c2', detail: '' }],
        }),
        k: 't8',
      },
    }).body;

    expect(drawn, 'the reason rides the row').toContain('fam-tail');
    expect(drawn, 'in its own words').toContain('no task with id t-91c2');
    expect(drawn, 'and the body keeps the whole of it').toContain('errhint');
  });

  it('draws a card-supplied tail, which is a clean answer and not a failure', () => {
    // A blocked despawn answers cleanly; the tail is the card's, and the
    // row's own `failed` styling must not be needed for it to draw.
    const drawn = render(Call, {
      props: {
        call: tasksUpdate({
          forge: {
            title: "worker 'implementer' still live",
            chips: [],
            figure: null,
            pieces: [],
            tail: { text: '3 uncommitted files', tone: 'warn' },
          },
        }),
        k: 't10',
      },
    }).body;

    expect(drawn, 'the reason rides the row').toContain('3 uncommitted files');
    expect(drawn, 'in the warn tone the card stated').toContain('fam-tail warn');
  });

  it('draws the meter a card asks for, where its numbers are', () => {
    const drawn = render(Call, {
      props: {
        call: tasksUpdate({
          row: { kind: 'forge', family: 'agents' },
          name: 'mcp__forge__agents__capacity',
          forge: {
            title: 'worker capacity',
            chips: [
              { text: '7 live', tone: 'plain' },
              { text: 'cap 8', tone: 'dim' },
            ],
            figure: '1 free',
            pieces: [],
            meter: { fill: 7, of: 8 },
            glyph: 'gauge',
          },
        }),
        k: 't11',
      },
    }).body;

    expect(drawn, 'the bar draws').toContain('fam-meter');
    expect(drawn, 'filled to the count against the cap').toContain('width: 88%');
    expect(drawn, 'with both numbers still stated in words').toContain('7 live');
    expect(drawn, "the card's own mark, not the family's").toContain('href="#i-gauge"');
  });

  it('keeps the reason off every other failed row', () => {
    // Ved's shape: the tail is the MCP family cards' own, and a generic call
    // row keeps its reason in the body alone.
    const drawn = render(Call, {
      props: {
        call: {
          ...tasksUpdate(),
          row: { kind: 'family', family: 'bash' },
          name: 'Bash',
          status: 'failed',
          forge: null,
          body: [{ kind: 'error', message: 'exit code 1', detail: '' }],
        } as ToolLeaf,
        k: 't9',
      },
    }).body;

    expect(drawn, 'the reason draws in the body').toContain('exit code 1');
    expect(drawn, 'and not on the row').not.toContain('fam-tail');
  });
});

describe('the dispatch row, joined to its instance', () => {
  /**
   * A `Task` dispatch, as the fold hands it to the row: the call itself is
   * COMPLETED - the CLI's launch-ack answered it in a second - while the
   * instance it opened runs on for minutes.
   */
  const dispatch = (): ToolLeaf => ({
    id: 'toolu_task',
    row: { kind: 'family', family: 'tool' },
    name: 'Task',
    title: 'review the fold',
    command: null,
    status: 'completed',
    note: null,
    body: [{ kind: 'text', text: 'Report: **closed**.' }],
    mutation: null,
    decision: null,
    forge: null,
    skill: null,
    image: null,
    imageNote: null,
  });

  const card = (over: Partial<SubagentCard> = {}): SubagentCard => ({
    name: 'review the fold',
    dispatch_id: 'toolu_task',
    agent_type: 'code-reviewer',
    running: true,
    failed: false,
    backgrounded: false,
    ended_at: null,
    calls: 3,
    tail: [],
    usage: { total_tokens: 12_000, tool_uses: 3, duration_ms: 184_000 },
    ...over,
  });

  /** The dispatch itself, as the session's own assistant frame carries it. */
  const dispatchFrame = () => ({
    type: 'assistant',
    parent_tool_use_id: null,
    message: {
      role: 'assistant',
      content: [
        {
          type: 'tool_use',
          id: 'toolu_task',
          name: 'Task',
          input: {
            description: 'review the fold',
            prompt: 'do the thing',
            subagent_type: 'code-reviewer',
          },
        },
      ],
    },
  });

  it('draws the instance running while the settled call would have said done', () => {
    subagents.sync([card({ backgrounded: true })]);
    const drawn = render(Call, { props: { call: dispatch(), k: 'task' } }).body;

    expect(drawn, 'the loader is the liveness of the instance, not the call').toContain(
      '<span class="ring"></span>',
    );
    expect(drawn, 'the agent type rides the row').toContain('code-reviewer');
    expect(drawn, 'and the background chip does').toContain('>background<');
    expect(drawn, 'no figures while it runs').not.toContain('sg-fig');

    subagents.sync(null);
  });

  it('draws the figures once the instance settles', () => {
    subagents.sync([card({ running: false })]);
    const drawn = render(Call, { props: { call: dispatch(), k: 'task' } }).body;

    expect(drawn, 'the settled figures').toContain('3 calls \u{b7} 12.0k tokens \u{b7} 3m 04s');
    expect(drawn).not.toContain('<span class="ring"></span>');

    subagents.sync(null);
  });

  it('stops a backgrounded instance at its own report', () => {
    // Its own text is the launch ack, which says nothing a reader acts on -
    // so no output block and no transcript path - while its task facts stay,
    // the same line a foreground row carries minus the path.
    subagents.sync([card({ backgrounded: true, running: false })]);
    const drawn = render(Call, {
      props: {
        call: dispatch(),
        k: 'task',
        open: true,
        messages: [
          dispatchFrame(),
          {
            type: 'system',
            subtype: 'task_started',
            tool_use_id: 'toolu_task',
            task_id: 't1',
            task_type: 'local_agent',
            spawn_depth: 1,
          },
        ],
      },
    }).body;

    expect(drawn, 'no output block').not.toContain('sg-result');
    expect(drawn, 'the task facts are drawn either way').toContain('sg-meta');
    expect(drawn, 'and the transcript path is not').not.toContain('output file');

    subagents.sync(null);
  });

  it('draws the instance own report once, not twice off the notification', () => {
    // The notification's summary echoes the agent's final message; the
    // timeline draws that message as its own prose, and a block drawn from
    // the summary too would show the whole report twice.
    subagents.sync([card({ running: false })]);
    const drawn = render(Call, {
      props: {
        call: dispatch(),
        k: 'task',
        open: true,
        messages: [
          dispatchFrame(),
          {
            type: 'assistant',
            parent_tool_use_id: 'toolu_task',
            message: {
              role: 'assistant',
              content: [{ type: 'text', text: 'unique-report-text' }],
            },
          },
          {
            type: 'system',
            subtype: 'task_notification',
            tool_use_id: 'toolu_task',
            task_id: 't1',
            status: 'completed',
            summary: 'unique-report-text',
          },
        ],
      },
    }).body;

    expect(drawn.split('unique-report-text').length - 1, 'the report draws once').toBe(1);

    subagents.sync(null);
  });

  it('crosses a failed instance even while the roster still calls it running', () => {
    // `failed` comes from the dispatch's answer and can land before the
    // roster settles the task: the cross leads the ring, so the row never
    // wears a spinner over work that already failed.
    subagents.sync([card({ running: true, failed: true })]);
    const drawn = render(Call, { props: { call: dispatch(), k: 'task' } }).body;

    expect(drawn, 'the cross draws').toContain('i-x');
    expect(drawn, 'and the ring does not').not.toContain('<span class="ring">');

    subagents.sync(null);
  });

  it('leaves a call that opened no instance exactly as it was', () => {
    // No card in the store: a plain call row, whose liveness is its own
    // status - which is what a pre-resume dispatch draws.
    subagents.sync(null);
    const drawn = render(Call, { props: { call: dispatch(), k: 'task' } }).body;

    expect(drawn, 'settled, so no loader').not.toContain('<span class="ring"></span>');
    expect(drawn, 'and none of the instance chrome').not.toContain('sg-ty');
    expect(drawn, 'nor figures').not.toContain('sg-fig');
  });

  it('opens onto the instance own timeline, read from the turn own frames', () => {
    subagents.sync([card({ running: true })]);
    const messages = [
      dispatchFrame(),
      {
        type: 'assistant',
        parent_tool_use_id: 'toolu_task',
        message: {
          role: 'assistant',
          content: [{ type: 'tool_use', id: 'c1', name: 'Grep', input: { pattern: 'subagent' } }],
        },
      },
      {
        type: 'user',
        parent_tool_use_id: 'toolu_task',
        message: {
          role: 'user',
          content: [{ type: 'tool_result', tool_use_id: 'c1', content: '3 matches' }],
        },
      },
      {
        type: 'assistant',
        parent_tool_use_id: 'toolu_task',
        message: {
          role: 'assistant',
          content: [{ type: 'text', text: 'Report: **two nits** on the fold.' }],
        },
      },
    ];

    const drawn = render(Call, {
      props: { call: dispatch(), k: 'task', open: true, messages },
    }).body;

    expect(drawn, 'the brief it was given').toContain('do the thing');
    expect(drawn, 'every call the frames hold, not just the card tail').toContain('subagent');
    expect(drawn, 'with what it came back with').toContain('3 matches');
    expect(drawn, 'the brief renders as markdown, structure and all').toContain('class="prose"');
    expect(drawn, 'and so does the prose the instance wrote between calls').toContain(
      '<strong>two nits</strong>',
    );
    expect(drawn, 'the result text renders as prose too, not a terminal box').toContain(
      '<strong>closed</strong>',
    );

    subagents.sync(null);
  });
});
