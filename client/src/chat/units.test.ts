import { describe, expect, it } from 'vitest';

import { cronNames } from './cron-names.svelte';
import { familyOf } from './families';
import {
  fold,
  type HookLeaf,
  type HookRun,
  type InboundLeaf,
  type PeerCard,
  type ThoughtLeaf,
  type Unit,
  type WorkRow,
} from './units';

/** An assistant frame carrying `content`. */
const said = (content: unknown[], extra: Record<string, unknown> = {}): unknown => ({
  type: 'assistant',
  uuid: 'a1',
  message: { id: 'm1', role: 'assistant', model: 'claude-opus-5', content },
  ...extra,
});

/** A user frame: where prompts, tool results and deliveries all arrive. */
const heard = (content: unknown[], extra: Record<string, unknown> = {}): unknown => ({
  type: 'user',
  uuid: 'u1',
  message: { role: 'user', content },
  ...extra,
});

const text = (value: string): unknown => ({ type: 'text', text: value });

const use = (id: string, name: string, input: unknown = {}): unknown => ({
  type: 'tool_use',
  id,
  name,
  input,
});

const result = (id: string, value = 'ok'): unknown => ({
  type: 'tool_result',
  tool_use_id: id,
  content: value,
  is_error: false,
});

/** One assistant frame carrying a single call, numbered so two never share an id. */
const call = (family: string, n = 0): unknown =>
  said([
    use(
      `toolu_${family}_${n}`,
      { read: 'Read', search: 'Grep', bash: 'Bash', edit: 'Edit' }[family] ?? family,
      { file_path: 'src/lib.rs', command: 'just check' },
    ),
  ]);

/** The kinds a fold produced, in order. */
const kinds = (units: Unit[]): string[] => units.map((unit) => unit.kind);

/** The rows of the one work unit, or none for every other kind of unit. */
const rowsOf = (unit: Unit | undefined): WorkRow[] => (unit?.kind === 'leaves' ? unit.rows : []);

/** The call rows of a unit, in the order they arrived. */
const callsOf = (unit: Unit | undefined): Extract<WorkRow, { tag: 'call' }>[] =>
  rowsOf(unit).filter((row): row is Extract<WorkRow, { tag: 'call' }> => row.tag === 'call');

/** The peer cards of a unit, in the order they arrived. */
const cardsOf = (unit: Unit | undefined): PeerCard[] =>
  rowsOf(unit).flatMap((row) => (row.tag === 'card' ? [row.card] : []));

/** The thought rows of a unit, in the order they arrived. */
const thoughtsOf = (unit: Unit | undefined): ThoughtLeaf[] =>
  rowsOf(unit).flatMap((row) => (row.tag === 'thought' ? [row] : []));

/** The inbound rows of a unit, in the order they arrived. */
const inboundsOf = (unit: Unit | undefined): InboundLeaf[] =>
  rowsOf(unit).flatMap((row) => (row.tag === 'inbound' ? [row] : []));

/** The hook runs a fold drew, in the order they arrived. */
const runsOf = (units: Unit[]): HookLeaf[] =>
  units.flatMap((unit) => rowsOf(unit).flatMap((row) => (row.tag === 'hook' ? [row] : [])));

/** The first hook run a fold drew, or null when it drew none. */
const runOf = (units: Unit[]): HookRun | null => runsOf(units)[0]?.run ?? null;

/**
 * One hook's run, as the CLI's own capture sends it
 * (`crates/forge-test-harness/baselines/sdk/2.1.280/compact.jsonl`): a start,
 * two progress frames whose output is cumulative, and the response that
 * settles it.
 *
 * **Every name here is the wire's own**, taken off that row rather than off
 * `Message`'s Rust fields - the fold reads JSON with no type link to the
 * crate, so a serde rename stops matching with nothing to compile against.
 */
const hookFrames = (): unknown[] => [
  {
    type: 'system',
    subtype: 'hook_started',
    hook_id: '976fd1c5-cc6a-4c3b-9e80-91d52aeb00c7',
    hook_name: 'SessionStart:startup',
    hook_event: 'SessionStart',
    uuid: '47e5b877-93aa-4b3f-a44b-a993cefeb201',
    session_id: '41e36bf9-8391-44c4-95fe-6167881d7f73',
  },
  {
    type: 'system',
    subtype: 'hook_progress',
    hook_id: '976fd1c5-cc6a-4c3b-9e80-91d52aeb00c7',
    hook_name: 'SessionStart:startup',
    hook_event: 'SessionStart',
    stdout: 'capture-line-1\n',
    stderr: '',
    output: 'capture-line-1\n',
    uuid: 'f33f2f94-cf92-4f3d-a322-997470671dd5',
    session_id: '41e36bf9-8391-44c4-95fe-6167881d7f73',
  },
  {
    type: 'system',
    subtype: 'hook_progress',
    hook_id: '976fd1c5-cc6a-4c3b-9e80-91d52aeb00c7',
    hook_name: 'SessionStart:startup',
    hook_event: 'SessionStart',
    stdout: 'capture-line-1\ncapture-line-2\n',
    stderr: '',
    output: 'capture-line-1\ncapture-line-2\n',
    uuid: '45d4e988-4458-470d-a4d8-30d11dee6fd7',
    session_id: '41e36bf9-8391-44c4-95fe-6167881d7f73',
  },
  {
    type: 'system',
    subtype: 'hook_response',
    hook_id: '976fd1c5-cc6a-4c3b-9e80-91d52aeb00c7',
    hook_name: 'SessionStart:startup',
    hook_event: 'SessionStart',
    output: '<redacted-hook-body>',
    stdout: '<redacted-hook-body>',
    stderr: '',
    exit_code: 0,
    outcome: 'success',
    uuid: '1814e038-fc4b-449e-a5ff-b35952baa418',
    session_id: '41e36bf9-8391-44c4-95fe-6167881d7f73',
  },
];

describe('one turn folded into the units a view draws', () => {
  it('folds consecutive calls into one list', () => {
    // The rule the mockup was built on and the one a reader gets wrong first:
    // a run of calls between two prompts is ONE unit, not one row each - and
    // the rows inside it keep their arrival order.
    const units = fold([call('read', 0), call('search', 1), call('read', 2)]);

    expect(units).toHaveLength(1);
    const [group] = units;
    expect(group?.kind).toBe('leaves');
    // Arrival order, not grouped: the read, the search, then the read again.
    expect(callsOf(group).map((call) => familyOf(call.leaf.name))).toEqual([
      'read',
      'search',
      'read',
    ]);
  });

  it('draws the rows in arrival order, and the same order on a re-read', () => {
    // The rows never reorder, and the order derives from the call sequence, so
    // a page reopened from the transcript draws what the live one drew.
    const frames = [call('read', 0), call('search', 1), call('bash', 2), call('search', 3)];
    const order = (units: Unit[]): string[] =>
      callsOf(units[0]).map((call) => familyOf(call.leaf.name));

    expect(order(fold(frames)), 'every row keeps its arrival place').toEqual([
      'read',
      'search',
      'bash',
      'search',
    ]);
    expect(order(fold(structuredClone(frames))), 'and a re-read derives the same').toEqual(
      order(fold(frames)),
    );
  });

  it('keys each call by the fold own name, which an id-less block cannot collide on', () => {
    // The view keys the rows by this, and the wire's tool_use id is what
    // names a call - but the wire does not always give one. A row keyed on an
    // empty id is a duplicate the moment a family holds two such calls, and a
    // duplicate key stops the whole turn drawing at mount. So a block without
    // an id takes the frame and block it arrived in, which cannot move, and the
    // view lists by that rather than by the id alone.
    const idLess = (uuid: string): unknown =>
      said([{ type: 'tool_use', name: 'Bash', input: { command: 'just check' } }], { uuid });
    const units = fold([
      idLess('a1'),
      idLess('a2'),
      said([use('toolu_01', 'Bash', { command: 'just check' })], { uuid: 'a3' }),
    ]);

    const calls = callsOf(units[0]);

    expect(
      calls.map((call) => call.key),
      'two id-less calls take their frames, and a named one takes the fold own name',
    ).toEqual(['a1#0', 'a2#0', 'c-toolu_01']);
  });

  it('titles a failed forge call by its subject, never by the tool', () => {
    // **An MCP tool's own error is plain text**: `is_error` with no
    // `<tool_use_error>` envelope, which is the shape the CLI wraps only its
    // own refusals in. A row that titled itself from the tool would read
    // `forge: slack__list`, and one that looked only for the envelope would
    // draw no reason at all.
    const refused = (name: string, input: unknown, reason: string): unknown[] => [
      said([use('toolu_f', name, input)]),
      heard([{ type: 'tool_result', tool_use_id: 'toolu_f', content: reason, is_error: true }], {
        uuid: 'u-result',
      }),
    ];

    const units = fold(
      refused(
        'mcp__forge__slack__list',
        {},
        'several Slack workspaces are configured; pass `workspace`: Subspace, Trust Machines',
      ),
    );

    const [call] = callsOf(units[0]);
    expect(call?.leaf.title, 'the family own noun, not the tool').toBe('conversations');
    expect(
      call?.leaf.body.map((piece) => (piece.kind === 'text' ? piece.text : '')).join(''),
      'and the reason is there for the row and the body',
    ).toContain('several Slack workspaces are configured');
  });

  it('titles a successful forge call by the card own subject, not the family own noun', () => {
    // The SUCCESS arm of the title chain: a card that parses wins over the
    // failure fallback and over the tool name, so a reorder that dropped the
    // card's own title would silently title every successful update `tasks`.
    const record = JSON.stringify({
      id: 't-1',
      project: 'forge',
      subject: 'Sweep the stale worktrees',
      status: 'in_progress',
      owner: 'lead',
      estimate: null,
      detail: null,
      artifact: null,
      active_form: null,
      created_at: '2026-10-06T00:12:00Z',
      updated_at: '2026-10-06T09:30:00Z',
    });

    // A forge tool's result arrives as BLOCKS, not as a bare string - the
    // shape the wire carries for the MCP tools and not the one the built-ins
    // use, which is why `parsedText` reads blocks.
    const units = fold([
      said([use('toolu_ok', 'mcp__forge__tasks__update', { id: 't-1', status: 'in_progress' })]),
      heard(
        [
          {
            type: 'tool_result',
            tool_use_id: 'toolu_ok',
            content: [{ type: 'text', text: record }],
            is_error: false,
          },
        ],
        { uuid: 'u-ok' },
      ),
    ]);

    expect(callsOf(units[0])[0]?.leaf.title, 'the record own subject').toBe(
      'Sweep the stale worktrees',
    );
  });

  it('folds a mutation into the run as its own family', () => {
    const units = fold([call('read', 0), call('edit', 1), call('read', 2)]);

    expect(units, 'the mutation does not break the run').toHaveLength(1);
    const [group] = units;
    // The mutation sits between the reads, in arrival order.
    expect(callsOf(group).map((call) => familyOf(call.leaf.name))).toEqual([
      'read',
      'edit',
      'read',
    ]);
  });

  it('splits the run on the calls the mockup draws alone', () => {
    // An answered question is a cutter: while it waits it draws nothing here -
    // the dock is its row - and the card that splits the run is the record of
    // the answer. A peer message is NOT one, which has a test of its own.
    const question = [
      said([
        use('toolu_q', 'AskUserQuestion', {
          questions: [{ question: 'Which one?', options: [{ label: 'a' }] }],
        }),
      ]),
      heard([result('toolu_q', 'answered')], {
        tool_use_result: { answers: { 'Which one?': 'a' } },
      }),
    ];

    const units = fold([call('read', 0), ...question, call('read', 1)]);
    expect(units, 'the run splits around a call drawn on its own').toHaveLength(3);
    expect(kinds(units)[1], 'and that call is the unit in the middle').toBe('question');
  });

  it('keeps a peer message from closing the calls around it, as rows of one list', () => {
    // A message is a row, not a separator: the call below it joins the list
    // the call above it opened, and every row keeps its arrival place.
    const peer = [
      said([use('toolu_p', 'mcp__forge__agents__send_message', { project: 'x', message: 'hi' })]),
    ];

    const units = fold([call('read', 0), ...peer, call('read', 1)]);

    expect(units, 'one unit rather than three').toHaveLength(1);
    const [group] = units;
    expect(
      group?.kind === 'leaves' ? group.rows.map((row) => row.tag) : [],
      'the message arrived between the calls and draws between them',
    ).toEqual(['call', 'card', 'call']);
    expect(callsOf(group).length, 'and both calls drew').toBe(2);
    expect(cardsOf(group).length).toBe(1);
  });

  it('draws what the model thought, which the wire carries and nothing drew', () => {
    // The body is on the wire - `ContentBlock::Thinking { thinking, signature }`
    // in primitives, and a real transcript holds one - and this fold had no arm
    // for it, so the block fell through and the words were dropped. An empty
    // thinking draws nothing, the way the terminal skips one.
    const thought = said([{ type: 'thinking', thinking: 'the model wondered', signature: 'sig' }]);
    const empty = said([{ type: 'thinking', thinking: '', signature: 'sig' }]);

    const units = fold([thought]);
    expect(kinds(units), 'the thinking is a row rather than a drop').toEqual(['leaves']);
    expect(thoughtsOf(units[0])[0]?.text, 'words and all').toBe('the model wondered');
    expect(thoughtsOf(units[0])[0]?.key, 'keyed by the frame and block they came from').toBe(
      'a1#0',
    );
    expect(kinds(fold([empty])), 'and an empty one is not a row').toEqual([]);
  });

  /**
   * The compaction boundary, which the CLI sends and this fold dropped.
   *
   * A recorded boundary is a `system` frame of its own subtype, and the system
   * arm read `thinking_tokens` and `stop_hook_summary` and fell off the end for
   * everything else - so the frame that says where the conversation was cut
   * drew nothing, which is a frame dropped rather than a shape chosen.
   */
  it('draws the compaction boundary, which the wire sends and nothing drew', () => {
    const boundary = {
      type: 'system',
      subtype: 'compact_boundary',
      uuid: 'cb-1',
      session_id: 's1',
      compact_metadata: { trigger: 'auto', pre_tokens: 68_031, post_tokens: 9_149 },
    };

    const units = fold([boundary]);

    expect(kinds(units), 'the boundary is a row rather than a drop').toEqual(['compaction']);
    const [row] = units;
    expect(row?.kind === 'compaction' ? row.trigger : null, 'the trigger rides it').toBe('auto');
    expect(
      row?.kind === 'compaction' ? row.preTokens : null,
      'with the count read before the cut',
    ).toBe(68_031);
    expect(
      row?.kind === 'compaction' ? row.postTokens : null,
      'and the one carried after it, which is the third fact the wire carries',
    ).toBe(9_149);
  });

  it("hangs a skill's body on the call that loaded it, not on a row of its own", () => {
    // The call that loaded it already has its row; the body follows as a user
    // frame, and attaching it there is what makes that row open onto the
    // skill - a second row beside it says the same thing twice.
    const load = (skill: string): unknown => said([use(`toolu_${skill}`, 'Skill', { skill })]);
    const body = heard([
      text(
        'Base directory for this skill: /Users/ved/.claude/skills/unslop\n\n# Unslop\n\nEdit text.',
      ),
    ]);
    const cached = heard([
      text(
        'Base directory for this skill: /Users/ved/.claude/plugins/cache/ui-ux-pro-max-skill/ui-ux-pro-max/2.13.0\n\n# Ux\n\nDo it.',
      ),
    ]);

    const units = fold([load('unslop'), body]);
    expect(kinds(units), 'the call unit alone, no second row').toEqual(['leaves']);
    const [group] = units;
    const held = callsOf(group).map((call) => call.leaf);
    expect(held, 'one call drew').toHaveLength(1);
    expect(held[0]?.skill, "carrying the skill's own words").toBe('# Unslop\n\nEdit text.');

    const [plugin] = fold([load('ui-ux-pro-max:ui-ux-pro-max'), cached]);
    expect(
      plugin?.kind === 'leaves' ? callsOf(plugin)[0]?.leaf.skill : null,
      'and a plugin skill matches though the two spellings differ',
    ).toBe('# Ux\n\nDo it.');
  });

  it('hangs a body the CLI marked synthetic on its call, without the plumbing line', () => {
    // **The mark, not the text** (#1543): a launched skill's body can arrive
    // with neither the plumbing line nor a matching heading - the timesheet
    // fill's did - and the one signal every injected body carries is the
    // synthetic mark. It rides the call that loaded the skill, by position,
    // and never draws as the reader's own turn.
    const load = said([use('toolu_fill', 'Skill', { skill: 'timesheet-fill' })]);
    // Neither recognizer's shape: no plumbing line, and no heading either -
    // the fresh case drew as a turn precisely because both missed it.
    const body = heard([text('You are filling a timesheet. Ask for the week first.')], {
      isSynthetic: true,
    });

    const units = fold([load, body]);
    expect(kinds(units), 'the call unit alone, no turn of the reader').toEqual(['leaves']);
    const [group] = units;
    const held = callsOf(group).map((call) => call.leaf);
    expect(held[0]?.skill, 'the call opens onto the body the mark claims').toContain(
      'You are filling a timesheet',
    );
  });

  it('draws an unclaimed synthetic body as a notice, never as the reader', () => {
    const body = heard([text('An injected line with no call behind it.')], { isSynthetic: true });
    const units = fold([body]);

    expect(kinds(units), "a synthetic frame drew as the reader's own").toEqual(['notice']);
  });

  it('leaves a stamped heading with no matching call off a call that is waiting', () => {
    // Only the nameless marked body pairs by position; a named frame that
    // matches no waiting call must never ride one that happens to be waiting.
    const load = said([use('toolu_alpha', 'Skill', { skill: 'alpha-skill' })]);
    const heading = heard([text('# Beta Report\n\nBeta body words.')], { isSynthetic: true });

    const units = fold([load, heading]);
    const [group] = units;
    const held = callsOf(group).map((call) => call.leaf);
    expect(held[0]?.skill, 'the waiting call keeps waiting').toBeNull();
    expect(kinds(units), 'and the heading falls through as an ordinary frame').toEqual([
      'leaves',
      'user',
    ]);
  });

  it('claims nothing for a whitespace-only stamped frame', () => {
    const load = said([use('toolu_alpha', 'Skill', { skill: 'alpha-skill' })]);
    const blank = heard([text('   \n\t  ')], { isSynthetic: true });

    const units = fold([load, blank]);
    const [group] = units;
    const held = callsOf(group).map((call) => call.leaf);
    expect(held[0]?.skill, 'a whitespace-only stamped frame claims nothing').toBeNull();
  });

  it('takes the mark as the harness talking, and an unmarked frame as the reader', () => {
    const plain = heard([text('the reader typed this')]);
    const stamped = heard([text('a line nobody typed, and no family claims it')], {
      isSynthetic: true,
    });

    expect(kinds(fold([plain])), 'an ordinary frame still draws as the reader').toEqual(['user']);
    expect(kinds(fold([stamped])), "a stamped frame drew as the reader's own").toEqual(['notice']);
  });

  it('draws nothing for the local-command family, by decision', () => {
    // The reader's typing in the LAUNCH terminal arrives as plumbing, and the
    // terminal's own chat filters the same heads. Ved's ruling, 2026-10-03:
    // ignored deliberately, which is why this test pins the silence.
    const ignored = [
      '<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>',
      '<command-name>/clear</command-name>\n            <command-message>clear</command-message>\n            <command-args></command-args>',
      '<command-message>handoff</command-message>\n<command-name>/handoff</command-name>',
      '<local-command-stdout>Set model to `gpt-5.6-luna`</local-command-stdout>',
    ];

    for (const held of ignored) {
      expect(kinds(fold([heard([text(held)])])), `nothing draws for ${held.slice(0, 24)}`).toEqual(
        [],
      );
    }
    // The control: ordinary words still draw as the reader's own turn.
    expect(kinds(fold([heard([text('a real prompt')])]))).toEqual(['user']);
  });

  it('draws a background task end as its summary line, never the XML', () => {
    // The CLI delivers a task's end as a user frame nobody typed - the
    // `<task-notification>` envelope - so it draws as an info line of its
    // own: not the raw XML, and not the reader's own turn (#1680, Ved's
    // shape: the summary line alone).
    const envelope = heard([
      text(
        '<task-notification>\n<task-id>bsybtnmwj</task-id>\n<tool-use-id>call_01a1</tool-use-id>\n<output-file>/tmp/x.output</output-file>\n<status>completed</status>\n<summary>Background command "Run both gates" completed (exit code 0)</summary>\n</task-notification>',
      ),
    ]);

    const units = fold([envelope]);
    expect(kinds(units), 'not a turn the reader took').toEqual(['notice']);
    const [row] = units;
    expect(row?.kind === 'notice' ? row.notice.severity : '', 'informational').toBe('info');
    expect(
      row?.kind === 'notice' ? row.notice.text : '',
      'the summary line alone, no envelope around it',
    ).toBe('Background command "Run both gates" completed (exit code 0)');

    // No summary parsed, no claim: the frame draws as itself, the rule-25
    // default rather than a silent drop.
    const bare = heard([text('<task-notification><task-id>t</task-id></task-notification>')]);
    expect(kinds(fold([bare])), 'a summary-less envelope is not claimed').toEqual(['user']);
  });

  it('draws the invisible-output nudge as the page own line, never the bracket', () => {
    // The harness asks the MODEL to continue after a response with no visible
    // output - a marked frame nobody typed - and the terminal draws the raw
    // bracket; the page draws its own line (#1858).
    const nudge = heard(
      [
        text(
          '[Your previous response had no visible output. Please continue and produce a user-visible response.]',
        ),
      ],
      { isSynthetic: true },
    );
    const units = fold([nudge]);

    expect(kinds(units), 'not a turn the reader took').toEqual(['notice']);
    const [row] = units;
    expect(
      row?.kind === 'notice' ? row.notice.text : '',
      'the page own words, not the raw bracket',
    ).toBe('no visible output - the harness asked the agent to continue');

    // A bracket this does not recognize falls through: with the stamp it
    // draws as the raw marked line, the shape it had before.
    const odd = heard([text('[Some other bracketed sentence entirely.]')], { isSynthetic: true });
    const [held] = fold([odd]);
    expect(
      held?.kind === 'notice' ? held.notice.text : '',
      'an unknown bracket draws as itself',
    ).toContain('[Some other bracketed sentence entirely.]');
  });

  it("hangs the harness's image note on the call that read the picture", () => {
    // The image rides the Read call's own result; the note arrives right after
    // as a user frame. On that call's row it is the picture's caption; as the
    // reader's turn it wears an attribution nobody earned.
    const read = said([use('toolu_shot', 'Read', { file_path: '/Users/ved/shot.png' })]);
    const picture = heard([
      {
        type: 'tool_result',
        tool_use_id: 'toolu_shot',
        content: [
          { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AAAA' } },
        ],
      },
    ]);
    // Stamped as synthetic, the way the wire carries it - the mark is why a
    // reader of the fold has to claim it HERE, before the mark's own branch.
    const note = heard(
      [
        text(
          '[Image: original 2782x1034, displayed at 2000x743. Multiply coordinates by 1.39 to map to original image.]',
        ),
      ],
      { isSynthetic: true },
    );

    const units = fold([read, picture, note]);
    const [group] = units;
    const held = group?.kind === 'leaves' ? callsOf(group)[0]?.leaf : undefined;

    expect(held?.image, 'the picture is on the call that read it').toEqual({
      mime: 'image/png',
      data: 'AAAA',
    });
    expect(held?.imageNote, "and the harness's line is its caption").toContain(
      'original 2782x1034',
    );
    expect(kinds(units), 'nothing of the reader draws here').toEqual(['leaves']);

    // A note with no picture behind it still draws, as a line of its own -
    // stamped like the rest, so the claim order is what the mark branch sees.
    const orphan = fold([
      heard(
        [
          text(
            '[Image: original 100x100, displayed at 100x100. Multiply coordinates by 1.00 to map to original image.]',
          ),
        ],
        { isSynthetic: true },
      ),
    ]);
    expect(kinds(orphan), 'a note nothing holds draws a notice').toEqual(['notice']);
  });

  it("hangs a tool-invoked skill's titled body on its call as well", () => {
    // The other carrier: when the Skill tool loads one, the body IS the
    // skill's markdown, opening on `# PR Review Loop` with no plumbing line -
    // named by that heading, which is a different spelling of the call's
    // `pr-review-loop` and has to match it all the same.
    const load = said([use('toolu_pr', 'Skill', { skill: 'pr-review-loop' })]);
    const body = heard([
      text('# PR Review Loop\n\nReview a change with parallel specialist reviewers.'),
    ]);

    const units = fold([load, body]);
    expect(kinds(units), 'nothing draws as the reader').toEqual(['leaves']);
    const [group] = units;
    const held = group?.kind === 'leaves' ? callsOf(group)[0]?.leaf : undefined;
    expect(held?.skill, "the call's row opens onto the skill").toContain('# PR Review Loop');
  });

  it('draws an unclaimed skill body as its own row rather than as the reader own turn', () => {
    // The CLI injects a skill's body as a user frame and nobody typed it; the
    // row is built from the frame's own first line, which is the only marker
    // the wire carries, and that line is dropped from the body.
    const skills = heard([
      text(
        'Base directory for this skill: /Users/ved/.claude/skills/unslop\n\n# Unslop\n\nEdit text.',
      ),
    ]);
    const plugin = heard([
      text(
        'Base directory for this skill: /Users/ved/.claude/plugins/cache/ui-ux-pro-max-skill/ui-ux-pro-max/2.13.0\n\n# Ux\n\nDo it.',
      ),
    ]);

    const units = fold([skills]);
    expect(kinds(units), 'a row of its own').toEqual(['skill']);
    const [row] = units;
    expect(row?.kind === 'skill' ? row.name : null, 'named off the path').toBe('unslop');
    expect(row?.kind === 'skill' ? row.body : '', 'the body without the plumbing line').toBe(
      '# Unslop\n\nEdit text.',
    );
    const [cached] = fold([plugin]);
    expect(
      cached?.kind === 'skill' ? cached.name : null,
      'and a versioned plugin path names the skill above the version',
    ).toBe('ui-ux-pro-max');
  });

  it('hangs the continuation prompt on the compaction row rather than the reader', () => {
    // The prompt is a user frame nobody typed, right after the boundary. Drawn
    // as the reader's it wore an attribution, and the compaction row opened
    // onto only the counts - the summary is the one account of what was cut.
    const boundary = {
      type: 'system',
      subtype: 'compact_boundary',
      uuid: 'cb-1',
      compact_metadata: { trigger: 'auto', pre_tokens: 68_031, post_tokens: 9_149 },
    };
    // Stamped as synthetic, the way the wire carries it: the continuation is
    // claimed by its own recognizer, so the mark must not reach it first.
    const summary = heard(
      [
        text(
          'This session is being continued from a previous conversation that ran out of context. And so on.',
        ),
      ],
      { isSynthetic: true },
    );

    const units = fold([boundary, summary]);
    expect(kinds(units), 'one row, not a turn beside it').toEqual(['compaction']);
    const [row] = units;
    expect(row?.kind === 'compaction' ? row.summary : null, 'the prompt rides the row').toContain(
      'This session is being continued',
    );
    expect(row?.kind === 'compaction' ? row.preTokens : null, 'with its facts kept').toBe(68_031);
  });

  it('draws a continuation prompt with no boundary as the compaction it is', () => {
    // The cut happened whether or not its frame reached this fold; the row
    // carries what it has, and the summary is what it has.
    const summary = heard(
      [
        text(
          'This session is being continued from a previous conversation that ran out of context. More.',
        ),
      ],
      { isSynthetic: true },
    );

    const units = fold([summary]);
    expect(kinds(units)).toEqual(['compaction']);
    const [row] = units;
    expect(row?.kind === 'compaction' ? row.trigger : 'x', 'no facts to draw').toBeNull();
    expect(row?.kind === 'compaction' ? row.summary : null).toContain('continued');
  });

  it('draws a boundary that carries no metadata, saying only that it happened', () => {
    // The bare shape comes from drift: a rename of the outer key (`compact
    // _metadata` on the wire, `compactMetadata` on disk) drops the frame to the
    // CLI's generic bucket with no metadata anywhere in it. The row is the
    // compaction itself, so it draws without its facts.
    const [row] = fold([{ type: 'system', subtype: 'compact_boundary', uuid: 'cb-2' }]);

    expect(row?.kind, 'the boundary still draws').toBe('compaction');
    expect(row?.kind === 'compaction' ? row.trigger : 'x').toBeNull();
    expect(row?.kind === 'compaction' ? row.preTokens : 0).toBeNull();
    expect(row?.kind === 'compaction' ? row.postTokens : 0).toBeNull();
  });

  it('keeps a run whole across a thinking row, and draws the thought where it landed', () => {
    // A thinking block is commentary ON the work rather than a separator
    // between pieces of it: the terminal has no thinking variant at all, so its
    // run cannot break on one. The regression was measured on a real turn:
    // drawing each thought as its own unit split one run of 24 calls into
    // twelve groups, where the same turn drew four. As a row it draws where it
    // arrived - between the two calls.
    const units = fold([
      call('read', 0),
      said([{ type: 'thinking', thinking: 'about the file', signature: 's' }]),
      call('read', 1),
    ]);

    expect(units, 'one unit, not a row beside it').toHaveLength(1);
    expect(kinds(units)).toEqual(['leaves']);
    const [group] = units;
    expect(
      group?.kind === 'leaves' ? group.rows.map((row) => row.tag) : [],
      'the thought sits where it arrived, between the calls',
    ).toEqual(['call', 'thought', 'call']);
    expect(thoughtsOf(group)[0]?.text, 'the words still draw').toBe('about the file');
    expect(callsOf(group).length, 'one run, not two').toBe(2);
  });

  it('keeps a message batch whole across a thinking row', () => {
    // The class list calls a run of peer messages a tool run, so the same rule
    // holds: a thought between two messages is commentary, not a separator. The
    // round measured thirteen of ninety-four real turns changing by exactly
    // this unit when the batch split.
    const tell = (n: number): unknown =>
      said([
        use(`toolu_tell_${n}`, 'mcp__forge__agents__send_message', {
          project: 'x',
          message: `m${n}`,
        }),
      ]);

    const units = fold([
      tell(1),
      said([{ type: 'thinking', thinking: 'between the messages', signature: 's' }]),
      tell(2),
    ]);

    expect(kinds(units), 'the thought is a row, not a unit of its own').toEqual(['leaves']);
    expect(units, 'one batch, not two').toHaveLength(1);
    const [group] = units;
    expect(group?.key, 'named by the first message, which is data the turn cannot move').toBe(
      'p-toolu_tell_1',
    );
    const cards = cardsOf(group);
    expect(cards, 'with both messages in it').toHaveLength(2);
    expect(thoughtsOf(group), 'and the words still draw').toHaveLength(1);
  });

  it('names a batch by its first card even when the wire gave the card no id', () => {
    // A card with no id in the data takes the frame and block it arrived in -
    // position, but one that cannot move - rather than keying its batch by an
    // empty string, where two such batches in one turn would collide. Three
    // sources build a card: a call, a header, and a queued prompt's envelope.
    const call = said([
      {
        type: 'tool_use',
        name: 'mcp__forge__agents__send_message',
        input: { project: 'x', message: 'm' },
      },
    ]);
    const envelope = heard([text("[Question id= from agent 'x' (org 'y')]\n\npicking it up")]);
    const queued = heard([
      {
        type: 'queued_command',
        prompt: "[Question id= from agent 'x' (org 'y')]\n\npicking it up",
      },
    ]);

    const fromCall = fold([call]);
    const fromEnvelope = fold([envelope]);
    const fromQueued = fold([queued]);

    expect(fromCall[0]?.key, 'the call card takes the frame and block').toBe('p-a1#0');
    expect(fromEnvelope[0]?.key, 'and so does a header with no id after the marker').toBe('p-u1#0');
    expect(fromQueued[0]?.key, 'and so does a queued prompt carrying one').toBe('p-u1#0');
  });

  it("draws the harness skill reminder as a line of its own, not the reader's turn", () => {
    // The CLI tells the MODEL that a skill was already loaded; nobody typed it.
    // The frame reaches the wire stamped, and the terminal draws every stamped
    // user frame as an info row on both paths, live and resume - so this is
    // parity, not the client's own shape, and rule 25 is why it draws at all.
    const reminder = heard([
      text(
        'Skill /unslop was loaded earlier (see the invoked-skills reminder above); this is a NEW invocation - follow those instructions now, including any setup steps.',
      ),
    ]);
    const units = fold([reminder]);

    expect(kinds(units), 'not a turn the reader took').toEqual(['notice']);
    expect(
      units[0]?.kind === 'notice' ? units[0].notice.severity : '',
      'informational, not a warning',
    ).toBe('info');
    expect(
      units[0]?.kind === 'notice' ? units[0].notice.text : '',
      'and the words survive',
    ).toContain('was loaded earlier');
  });

  it("draws the core's own line about a command, at the severity it carries", () => {
    const line = (severity: unknown): unknown => ({
      type: 'system',
      subtype: 'forge_notice',
      severity,
      text: 'Usage: /resume <session_id>',
    });

    const refused = fold([line('error')]);
    expect(kinds(refused)).toEqual(['notice']);
    expect(refused[0]?.kind === 'notice' ? refused[0].notice.severity : '').toBe('error');
    expect(refused[0]?.kind === 'notice' ? refused[0].notice.text : '').toContain('Usage: /resume');

    // The third level: a line this PAGE authors may carry it (the rate-limit
    // explainer), where the wire's own NoticeSeverity has only two.
    const warned = fold([line('warning')]);
    expect(warned[0]?.kind === 'notice' ? warned[0].notice.severity : '').toBe('warning');

    // A severity word this page does not know is not a failure: the line is
    // still drawn, and it says so quietly rather than shouting.
    const unknown = fold([line('catastrophe')]);
    expect(unknown[0]?.kind === 'notice' ? unknown[0].notice.severity : '').toBe('info');
  });

  it('draws a running turn row from the frames the turn already carries', () => {
    // While a turn runs, the frames carry usage on EVERY assistant message -
    // measured per call on a real session (`197i 1995o 638592r 0w`) and on a
    // saved page's own frames - so the running figures are a fold that never
    // read them, not a wire that never sent them. `live` is the caller's fact
    // and cannot be inferred from the frames: the server's saved page carries
    // no result frame either (the page fixture is 231 assistant and 130 user
    // rows, no result), so "no result" also means "read from disk".
    const spoke = (at: number): unknown => ({
      type: 'assistant',
      uuid: `a${at}`,
      timestamp: `2026-10-01T0${at}:00:00Z`,
      message: {
        id: `m${at}`,
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'text', text: 'working' }],
        usage: {
          input_tokens: 100 * at,
          output_tokens: 10 * at,
          cache_read_input_tokens: 1000 * at,
          cache_creation_input_tokens: 0,
        },
      },
    });
    const counter = { type: 'system', subtype: 'thinking_tokens', estimated_tokens_delta: 40 };
    const frames = [spoke(1), counter, spoke(2)];

    const running = fold(frames, null, true);
    const reports = running.filter((unit) => unit.kind === 'report');
    expect(reports, 'one row, and it is the running one').toHaveLength(1);
    const info = reports[0]?.kind === 'report' ? reports[0].info : null;
    expect(info?.running, 'marked running rather than settled').toBe(true);
    expect(info?.input_tokens, 'the input side sums across messages').toBe(300);
    expect(info?.output_tokens, 'and the down count arrives the same way').toBe(30);
    expect(info?.cache_read_tokens, 'so do the cache figures').toBe(3000);
    expect(info?.thinking_tokens, 'the deltas the fold already sums').toBe(40);
    expect(info?.duration_ms, 'the span the stamps already measure').toBe(3_600_000);
    expect(info?.session_cost_usd, 'the cumulative cost is settle-only').toBeNull();

    // The control: read as a page, the same frames carry no row at all.
    expect(
      fold(frames, null).filter((unit) => unit.kind === 'report'),
      'a page draws no running row',
    ).toHaveLength(0);

    // And a live turn whose result has landed draws only the settled row.
    const result = {
      type: 'result',
      uuid: 'r1',
      is_error: false,
      subtype: 'success',
      duration_ms: 1000,
      usage: { input_tokens: 1, output_tokens: 2 },
    };
    const settled = fold([spoke(1), result], null, true);
    const settledReports = settled.filter((unit) => unit.kind === 'report');
    expect(settledReports, 'the settled row, not a running one beside it').toHaveLength(1);
    expect(settledReports[0]?.kind === 'report' ? settledReports[0].info.running : true).toBe(
      false,
    );
  });

  it('counts a message once when the CLI repeats it once per content block', () => {
    // One API call is several assistant frames - one per content block - and
    // every one of them carries the whole call's usage block. A capture that
    // repeats a message shows it: 25 of the 48 shipped captures repeat one,
    // and `monitor_persistent_stream` has 11 frames over 7 messages. The
    // terminal keys by message id so a repeat overwrites (`LiveTurn::record`),
    // which is the count this fold has to match.
    const block = (id: string, part: number, tokens: number): unknown => ({
      type: 'assistant',
      uuid: `a${id}-${part}`,
      timestamp: '2026-10-01T01:00:00Z',
      message: {
        id,
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'text', text: 'working' }],
        usage: {
          input_tokens: tokens,
          output_tokens: tokens / 10,
          cache_read_input_tokens: tokens * 10,
          cache_creation_input_tokens: 0,
        },
      },
    });

    // `m1` drew three times, `m2` once: two calls, not four frames.
    const frames = [
      block('m1', 0, 100),
      block('m1', 1, 100),
      block('m1', 2, 100),
      block('m2', 0, 200),
    ];
    const running = fold(frames, null, true).filter((unit) => unit.kind === 'report');
    const info = running[0]?.kind === 'report' ? running[0].info : null;

    expect(info?.input_tokens, 'the two calls, not the four frames that drew them').toBe(300);
    expect(info?.output_tokens, 'and the down count reads off the same calls').toBe(30);
    expect(info?.cache_read_tokens, 'so do the cache figures').toBe(3000);
  });

  it('breaks the run on anything that is not a call', () => {
    const units = fold([call('read', 0), said([text('here it is')]), call('bash', 1)]);

    expect(kinds(units)).toEqual(['leaves', 'text', 'leaves']);
  });

  it('draws nothing for a monitor', () => {
    // The inspector owns monitors and the chat draws nothing for one, so a row
    // here would put a watcher in the conversation beside the calls it watches.
    const monitor = said([use('toolu_m', 'Monitor', { description: 'watch', command: 'tail -f' })]);
    const units = fold([call('read', 0), monitor, call('read', 1)]);

    expect(units).toHaveLength(1);
    const [group] = units;
    expect(callsOf(group).length, 'the two reads drew and the monitor did not').toBe(2);
  });

  it('draws nothing for a dispatched agent, and the same frames without one are the chat', () => {
    // A sub-agent's frames are the inspector's: drawn here they duplicate the
    // subagents section inside every turn that used one.
    //
    // **The wire marks them with `parent_tool_use_id`, which is the branch
    // that fires in production** - the field carries the dispatch's own
    // tool-use id, and null for the session's own frames. The control rides in
    // the same test: the same frames without it ARE the conversation, so a
    // fold that dropped everything could not pass.
    const prose = said([text('the second one failed')]);
    const child = said([text('the second one failed')], { parent_tool_use_id: 'toolu_dispatch' });

    expect(fold([child]), 'a dispatched frame draws nothing').toHaveLength(0);
    expect(fold([prose]), 'and the same frame is the conversation on its own').toHaveLength(1);
  });

  it('keeps a dispatched call out of the session group', () => {
    const dispatched = said([use('toolu_child', 'Grep', { pattern: 'x' })], {
      parent_tool_use_id: 'toolu_dispatch',
    });
    const units = fold([call('read', 0), dispatched, call('read', 1)]);

    expect(units).toHaveLength(1);
    const [group] = units;
    expect(
      callsOf(group).map((call) => familyOf(call.leaf.name)),
      'the dispatched grep is out, both reads are in',
    ).toEqual(['read', 'read']);
  });

  it("reads an empty parent id as the session's own frame", () => {
    // The terminal reads the same field with the same guard: a frame carrying
    // an empty id is not a dispatch, and the wire does send them.
    const blank = said([text("the session's own line")], { parent_tool_use_id: '' });

    expect(fold([blank])).toHaveLength(1);
  });

  it('draws peer traffic as cards in the one list, a lone message included', () => {
    // The strings are the producers' own, from `peers/types.rs`'s
    // `to_prose`: a test using a shape nothing writes is what let three
    // envelopes draw as the reader's own turn with the suite green.
    //
    // **And a lone message draws on its own**, where the terminal's own
    // `merge_messaging_groups` holds a group back until it holds two.
    const one = heard([
      text("[Message id=t-1 from agent 'steward' (org 'Busytools')]\n\nIT IMPORTED"),
    ]);
    const two = heard([
      text("[Message id=t-2 from agent 'planner' (org 'Busytools')]\n\npicking it up"),
    ]);

    expect(kinds(fold([one])), 'a lone message draws its own row').toEqual(['leaves']);
    expect(kinds(fold([one, two]))).toEqual(['leaves']);
    const [group] = fold([one, two]);
    expect(cardsOf(group).length, 'both cards drew').toBe(2);
  });

  it('reads a recorded question and reply as the one message row they became', () => {
    // A transcript recorded before the verbs were folded holds `Question` and
    // `Reply` headers, and reopening it replays them through this same fold.
    // Both trailers end with a clause inside the bracket - a question names the
    // tool to answer with, a reply says what it answers - so a matcher anchored
    // on the org clause's `)]` never fires and the envelope draws as the
    // reader's own turn: the orange panel with a raw header on screen.
    const question = heard([
      text(
        // replay-only: agents__tell
        "[Question id=q-1 from agent 'steward' (org 'Busytools') - reply with agents__tell in_reply_to=q-1]\n\nis the cron issue filed?",
      ),
    ]);
    const reply = heard([
      text(
        "[Reply id=t-2 from agent 'planner' (org 'Busytools') to your earlier ask]\n\ntaking the render half",
      ),
    ]);
    const message = heard([text("[Message id=m-3 from agent 'steward' (org 'Busytools')]\n\nFYI")]);

    const [group] = fold([question, reply, message]);
    const cards = cardsOf(group);

    expect(cards.length, 'every card draws in the one list').toBe(3);
    expect(cards.map((card) => card.row)).toEqual(['arrived', 'arrived', 'arrived']);
    expect(cards[0]?.body).toBe('is the cron issue filed?');
    expect(cards[0]?.peer).toBe('steward');
  });

  it('keeps every card in arrival order however the messages interleave', () => {
    // One list for every envelope kind: the direction and the verb are the
    // row's business, and each card keeps its arrival place.
    const message = (id: string, body: string): unknown =>
      heard([text(`[Message id=${id} from agent 'steward' (org 'Busytools')]\n\n${body}`)]);

    const [group] = fold([message('m-1', 'one'), message('m-2', 'two'), message('m-3', 'three')]);
    const cards = cardsOf(group);

    expect(
      cards.map((card) => card.body),
      'in the order they arrived',
    ).toEqual(['one', 'two', 'three']);
  });

  it('carries the id the message arrived with, which is what names its group', () => {
    // A handle whose uniqueness is not guaranteed is what has thrown twice in
    // this shape, and the wire's own id is the one field that separates two
    // messages from one sender.
    const arrived = heard([
      text("[Message id=m-9c1 from agent 'forge/steward' (org 'Busytools')]\n\nhi"),
    ]);
    const sent = said([
      {
        type: 'tool_use',
        id: 'toolu_01Bg',
        name: 'mcp__forge__agents__send_message',
        input: { project: 'forge', label: 'steward', message: 'hi' },
      },
    ]);
    const cardOf = (units: Unit[]): { id?: string } | undefined => cardsOf(units[0])[0];

    expect(cardOf(fold([arrived]))?.id, 'an envelope is named by its own id').toBe('m-9c1');
    expect(cardOf(fold([sent]))?.id, 'and a call by the id the wire gave it').toBe('toolu_01Bg');
  });

  it("settles each send's card from what it came back with", () => {
    // The card's own mark is what says how it went: a send that failed draws
    // the failure mark and one still out draws the ring.
    const ask = (id: string): unknown =>
      said([
        {
          type: 'tool_use',
          id,
          name: 'mcp__forge__agents__send_message',
          input: { project: 'forge', label: 'steward', message: 'is it filed?' },
        },
      ]);
    const answer = (id: string, failed: boolean): unknown =>
      heard([
        {
          type: 'tool_result',
          tool_use_id: id,
          content: failed ? 'the seat has no session' : 'yes',
          is_error: failed,
        },
      ]);
    const status = (units: Unit[]): string | null => cardsOf(units[0])[0]?.status ?? null;

    expect(status(fold([ask('toolu_a'), answer('toolu_a', false)])), 'a send that landed').toBe(
      'completed',
    );
    expect(status(fold([ask('toolu_b'), answer('toolu_b', true)])), 'one that did not').toBe(
      'failed',
    );
    expect(status(fold([ask('toolu_c')])), 'and one still out').toBe('in_progress');
  });

  it('marks the counterparty by class, and tags the org only when it is not the reader own', () => {
    // The reader is `forge/chat-kinds`: `forge/steward` is a worker in this
    // project, a bare name is another project's own agent, and the org tag is
    // the case the project name cannot settle - the same name under another
    // org. All three are the producer's own spellings.
    const here = heard([
      text("[Message id=t-1 from agent 'forge/steward' (org 'Busytools')]\n\nin this project"),
    ]);
    const other = heard([
      text("[Message id=t-2 from agent 'gateway-backend' (org 'Busytools')]\n\nanother project"),
    ]);
    const away = heard([
      text("[Message id=t-3 from agent 'gateway-backend' (org 'Gateway')]\n\nanother org"),
    ]);
    const self = { org: 'Busytools', project: 'forge', label: 'chat-kinds' };

    const [group] = fold([here, other, away], self);
    const cards = cardsOf(group) ?? [];

    expect(
      cards.map((card) => card.peer),
      'every row names the seat it is about',
    ).toEqual(['forge/steward', 'gateway-backend', 'gateway-backend']);
    expect(
      cards.map((card) => card.org),
      'and tags the org only where it is not the reader own',
    ).toEqual([null, null, 'Gateway']);
  });

  it('draws an external delivery as a row of its own kind, not as a turn of the reader', () => {
    // A Gotify body sits ONE newline after the bracket: a title line, then the
    // message. A cron wrapper lands its prompt after `]\n\n`. Each delivery
    // joins the work as a row, the way a family's calls do.
    const gotify = heard([text("[Gotify - app 'ci', priority 9]\nbuild failed\nrun 412")]);
    const cron = heard([text('[Cron]\n\nthe morning sweep')]);

    const units = fold([gotify, cron]);
    expect(kinds(units), 'both deliveries join one list').toEqual(['leaves']);
    const rows = inboundsOf(units[0]);
    expect(
      rows.map((row) => row.kind),
      'each row of its own kind, in arrival order',
    ).toEqual(['gotify', 'cron']);
    expect(rows[0]?.elevated, 'priority 9 is elevated').toBe(true);
    expect(rows[0]?.title, 'the app and its priority').toBe('ci \u{b7} priority 9');
    expect(rows[0]?.body, 'the whole of what arrived, title line first').toBe(
      'build failed\nrun 412',
    );
    expect(rows[1]?.title, "a cron fire leads with its prompt's first line").toBe(
      'the morning sweep',
    );
    expect(rows[1]?.body, 'no leading blank line').toBe('the morning sweep');
  });

  it('names a fired cron by its schedule when the delivery frame said which', () => {
    // The prose carries the prompt and never the schedule, so the name can
    // only come from the frame the delivery arrives with - joined here by the
    // prompt's own id, which both carry.
    const fired = heard([text('[Cron]\n\nsummarise overnight CI')], { uuid: 'p-77aa' });
    cronNames.remember('p-77aa', 'Morning summary');
    const named = inboundsOf(fold([fired])[0])[0];

    expect(named?.title, 'the schedule, not the prompt').toBe('Morning summary');
    expect(named?.body, 'and the whole prompt stays the body').toBe('summarise overnight CI');

    const unknown = heard([text('[Cron]\n\nsweep the queue')], { uuid: 'p-9901' });
    expect(
      inboundsOf(fold([unknown])[0])[0]?.title,
      'a fire whose frame was never seen falls back to the prompt first line',
    ).toBe('sweep the queue');

    // A schedule registered with no description keeps the fallback too: the
    // frame names nothing, and a row titled with nothing is worse than the
    // prompt it is already showing.
    const bare = heard([text('[Cron]\n\nrotate the logs')], { uuid: 'p-3377' });
    cronNames.remember('p-3377', '');
    expect(inboundsOf(fold([bare])[0])[0]?.title, 'an empty description is not a name').toBe(
      'rotate the logs',
    );
  });

  it('reads a Slack id the way the server reads one', () => {
    // The server's heuristic is an uppercase initial and nothing but uppercase
    // or digits after it - no length floor and no letter restriction - so a
    // client that tightened it would print an author where the terminal drops
    // the clause, or drop one where it prints.
    const line = (author: string): unknown =>
      heard([
        text(`[Slack - workspace 'Busytools', general] id C1 ts 1.2\n${author}: the gate is green`),
      ]);

    const named = inboundsOf(fold([line('steward')])[0])[0];
    expect(named?.title, 'a name is printed in the row title').toContain('steward');

    for (const id of ['U9', 'B09ABC123', 'C0C0T5E6RM1', 'DEPLOYS']) {
      const held = inboundsOf(fold([line(id)])[0])[0];
      expect(held?.title, `${id} is an id, not a name`).not.toContain(id);
    }
  });

  it('draws a Slack bundle as its rows, bare channel and all', () => {
    // The producer writes the conversation LABEL, not a `#`-prefixed channel -
    // `granite-staging-alerts`, `general` - so a matcher requiring `#` puts
    // every Slack message in the chat as the person's own words.
    const one = heard([
      text(
        "[Slack - workspace 'Busytools', granite-staging-alerts] id C1 ts 1.2\nsteward: the gate is green",
      ),
    ]);
    const bundle = heard([
      text(
        "[Slack - workspace 'Busytools', general] id C1 ts 1.2 (2 messages)\nsteward: first\nplanner: second",
      ),
    ]);

    const units = fold([one, bundle]);
    expect(kinds(units)).toEqual(['leaves']);
    const rows = inboundsOf(units[0]);
    expect(
      rows.map((row) => row.kind),
      'both deliveries are slack rows',
    ).toEqual(['slack', 'slack']);
    expect(rows).toHaveLength(2);
    expect(rows[0]?.title, 'the bare channel label rides the title').toContain(
      'granite-staging-alerts',
    );
    expect(rows[0]?.title).toContain('steward');
    expect(rows[0]?.body).toBe('the gate is green');
  });

  it('draws a failed delivery as the outgoing row it belongs to, and a failed spawn as a warning', () => {
    const delivery = heard([
      text("[Message to agent 'companies' (org 'Busytools') failed to deliver: channel closed]"),
    ]);
    const spawn = heard([text("[Worker 'planner' spawn failed id=w-1: ENOENT]")]);

    const units = fold([delivery, spawn]);
    expect(kinds(units), 'the failure joins the list; the spawn is a notice').toEqual([
      'leaves',
      'notice',
    ]);
    const card = cardsOf(units[0])[0];
    expect(card?.row, 'the delivery keeps the outgoing row, warn-toned').toBe('failed');
    expect(card?.peer).toBe('companies');
    expect(card?.status).toBe('failed');
    expect(card?.body, 'the words carry what went wrong').toContain('channel closed');
    expect(units[1]?.kind === 'notice' ? units[1].notice.severity : null).toBe('warning');
    expect(units[1]?.kind === 'notice' ? units[1].notice.text : '').toContain('ENOENT');
  });

  it('names only the seat being read as this session, and every other row by what it is', () => {
    // **The fold is where this is decided, and the label is half the address.**
    // A worker reading `agents__list` sees its own project's lead, its own row
    // and its siblings - and comparing on org and project alone would call all
    // of them `this session`. Only the seat the page draws names itself; its
    // project's own agent says whose agent it is, another project says that,
    // and a worker carries the phrase of its charter with its own activity.
    const listed = [
      {
        label: 'lead',
        project: 'forge',
        path: '/tmp/forge',
        status: 'running',
        slot: { org: 'Busytools', project: 'forge', label: 'lead' },
      },
      {
        label: 'reviewer',
        project: 'forge',
        charter: 'review the diff',
        status: 'Running',
        activity: 'Idle',
        slot: { org: 'Busytools', project: 'forge', label: 'reviewer' },
      },
      {
        label: 'client-dev',
        project: 'forge',
        charter:
          'Hold the client loop for this stretch of work, from the fold through to the page.\nMore.',
        status: 'Running',
        activity: 'Running',
        slot: { org: 'Busytools', project: 'forge', label: 'client-dev' },
      },
      {
        label: 'lead',
        project: 'companies',
        path: '/tmp/companies',
        status: 'sleeping',
        slot: { org: 'Busytools', project: 'companies', label: 'lead' },
      },
    ];
    const call = said([
      { type: 'tool_use', id: 'toolu_list', name: 'mcp__forge__agents__list', input: {} },
    ]);
    const answer = heard([
      {
        type: 'tool_result',
        tool_use_id: 'toolu_list',
        content: JSON.stringify(listed),
        is_error: false,
      },
    ]);

    const [group] = fold([call, answer], {
      org: 'Busytools',
      project: 'forge',
      label: 'reviewer',
    });
    const seats = cardsOf(group)[0]?.seats ?? [];

    expect(
      seats.map((seat) => seat.label),
      'every row drew',
    ).toEqual(['lead', 'reviewer', 'client-dev', 'lead']);
    expect(
      seats.map((seat) => seat.what),
      "the reader's own seat alone names itself",
    ).toEqual([
      "the project's own agent",
      'this session',
      'Hold the client loop for this stretch of work, from the fold\u{2026}',
      'another project',
    ]);
    expect(
      seats.map((seat) => seat.liveness),
      'a worker carries its activity, lowercased; an agent carries none',
    ).toEqual(['', 'idle', 'running', '']);

    // **The same four rows read by a LEAD**, which is what pins both halves of
    // the comparison. The reader's label is `lead` here, so a comparison on the
    // label alone would call `companies`' agent the seat being read; and its
    // project's workers share the org and project, so a comparison on those
    // alone would call all three of them that.
    const [asLead] = fold([call, answer], {
      org: 'Busytools',
      project: 'forge',
      label: 'lead',
    });
    const leadSeats = cardsOf(asLead)[0]?.seats ?? [];

    expect(
      leadSeats[3]?.what,
      "a row sharing the reader's label but not its project is another project, so the label alone is not the seat",
    ).toBe('another project');
    expect(
      leadSeats.slice(1, 3).map((seat) => seat.what),
      "and the reader's project's workers are not the seat either, so org and project alone are not it",
    ).toEqual([
      'review the diff',
      'Hold the client loop for this stretch of work, from the fold\u{2026}',
    ]);
  });

  it('shows the refusal a failed send came back with, not the message that was sent', () => {
    // The row's words read `failed to deliver: <reason>`, so the reason has to
    // be the refusal: a send that never reached anybody drawn with the sender's
    // own text under it says the message arrived.
    const call = said([
      {
        type: 'tool_use',
        id: 'toolu_x',
        name: 'mcp__forge__agents__send_message',
        input: { org: 'Gateway', project: 'companies', message: 'picking it up' },
      },
    ]);
    const refused = heard([
      {
        type: 'tool_result',
        tool_use_id: 'toolu_x',
        content: "no project 'companies' is configured under org 'Gateway'",
        is_error: true,
      },
    ]);

    const [group] = fold([call, refused]);
    const card = cardsOf(group)[0];

    expect(card?.row, 'the send draws its failure row').toBe('failed');
    expect(card?.body, 'with the refusal under it').toContain(
      "no project 'companies' is configured",
    );
    expect(card?.body, 'and not the words that were sent').not.toContain('picking it up');

    // A refusal that carried no words leaves the tail off rather than falling
    // back to the message: the row says `failed to deliver:` and nothing else,
    // where the sender's own text under it would read as the reason.
    const wordless = fold([
      said([
        {
          type: 'tool_use',
          id: 'toolu_y',
          name: 'mcp__forge__agents__send_message',
          input: { org: 'Gateway', project: 'companies', message: 'picking it up' },
        },
      ]),
      heard([{ type: 'tool_result', tool_use_id: 'toolu_y', content: '', is_error: true }]),
    ]);
    expect(cardsOf(wordless[0])[0]?.body, 'a wordless refusal adds nothing').toBe('');
  });

  it('reads the reason out of the CLI envelope a wrapped refusal arrives in', () => {
    // **The MCP shape wraps the refusal** in the CLI's own envelope, and with
    // only text pieces read the card drew `failed to deliver:` and nothing
    // else - the reason reached the page nowhere, which is the drop rule 25
    // forbids. The fold reads the envelope off failed results, so the card's
    // body has to read the piece that comes out of it.
    const [group] = fold([
      said([
        {
          type: 'tool_use',
          id: 'toolu_z',
          name: 'mcp__forge__agents__send_message',
          input: { org: 'Gateway', project: 'companies', message: 'picking it up' },
        },
      ]),
      heard([
        {
          type: 'tool_result',
          tool_use_id: 'toolu_z',
          content:
            "<tool_use_error>no project 'companies' is configured under org 'Gateway'</tool_use_error>",
          is_error: true,
        },
      ]),
    ]);
    const card = cardsOf(group)[0];

    expect(card?.row, 'the send draws its failure row').toBe('failed');
    expect(card?.body, 'the reason crosses the envelope').toContain(
      "no project 'companies' is configured",
    );
    expect(card?.body, 'and the tags do not').not.toContain('tool_use_error');
  });

  it('keys two failures from one seat apart, which the fold does by frame and block', () => {
    // **Two notices from one seat in a turn are ordinary**: a bucket of parked
    // messages is acked one notice per message, and a resumed transcript
    // replays them. A card that named itself by its sender would give both one
    // key, and Svelte refuses a duplicate key at mount - so the failure card
    // carries no id of its own and the fold names it by where it arrived.
    const failed = (uuid: string, line: string): unknown => heard([text(line)], { uuid });
    const header = "[Message to agent 'companies' (org 'Busytools') failed to deliver: ";

    const [group] = fold([
      failed('u1', `${header}channel closed]`),
      failed('u2', `${header}target session connection lost]`),
    ]);
    const cards = cardsOf(group) ?? [];

    expect(cards.length, 'both notices drew').toBe(2);
    expect(
      cards.map((card) => card.id),
      'each named by the frame it arrived in, not by the seat that sent it',
    ).toEqual(['u1#0', 'u2#0']);
  });

  it('reads a recorded failure header as the same failed row', () => {
    // A transcript recorded before the verbs were folded holds the `Ask …`
    // header; reopening it draws the failure it always drew.
    const recorded = heard([
      text("[Ask id=q-1 to agent 'companies' (org 'Busytools') failed to deliver: channel closed]"),
    ]);

    const [group] = fold([recorded]);
    const card = cardsOf(group)[0];
    expect(card?.row).toBe('failed');
    expect(card?.peer).toBe('companies');
    expect(card?.body).toContain('channel closed');
  });

  it('draws a question the assistant asked, with what was answered', () => {
    const asked = said([
      use('toolu_q', 'AskUserQuestion', {
        questions: [
          {
            question: 'Which colour do you prefer?',
            options: [{ label: 'Red' }, { label: 'Blue' }],
          },
        ],
      }),
    ]);
    const answered = heard([result('toolu_q', 'answered')], {
      tool_use_result: { answers: { 'Which colour do you prefer?': 'Blue' } },
    });

    const units = fold([asked, answered]);
    expect(kinds(units)).toEqual(['question']);
    const [card] = units;
    const pairs = card?.kind === 'question' ? card.asked : [];
    expect(pairs[0]?.question).toBe('Which colour do you prefer?');
    expect(pairs[0]?.picked_labels).toEqual(['Blue']);
  });

  it('answers with what was typed, not the escape row it was typed through', () => {
    // The bug this pins: the annotation holds the reader's own words, and a
    // selected value that is not one of the question's own labels is the escape
    // row's label - so reading values first drew "you typed: Tell the agent
    // something else" where the words should have been.
    const asked = said([
      use('toolu_q', 'AskUserQuestion', {
        questions: [{ question: 'Which colour?', options: [{ label: 'Red' }, { label: 'Blue' }] }],
      }),
    ]);
    const answered = heard([result('toolu_q', 'answered')], {
      tool_use_result: {
        answers: { 'Which colour?': ['Tell the agent something else'] },
        annotations: { 'Which colour?': { notes: 'a teal, not listed' } },
      },
    });

    const units = fold([asked, answered]);
    const [card] = units;
    const pairs = card?.kind === 'question' ? card.asked : [];
    expect(pairs[0]?.typed_note, 'the escape row landed where the words go').toBe(
      'a teal, not listed',
    );
    expect(pairs[0]?.picked_labels, 'and nothing was picked').toEqual([]);
  });

  it('draws no question card until somebody answered it', () => {
    // The dock is the question's row while it waits, so a card carrying it too
    // drew the same prompt twice. It appears the moment an answer lands, which
    // the answer's own frame brings.
    const asked = said([
      use('toolu_q', 'AskUserQuestion', {
        questions: [{ question: 'Which colour?', options: [{ label: 'Red' }] }],
      }),
    ]);

    expect(fold([asked]), 'a waiting question drew a card').toEqual([]);
  });

  it('draws the turn hooks after the run they followed, and nothing when none fired', () => {
    // The captured row verbatim, less the synthetic parent id the fixture
    // carries (crates/forge-test-harness/baselines/sdk/2.1.280/real_session_sample.jsonl).
    // The two names are the wire's own - the message renames the Rust fields on
    // the way out - so a fold reading `actions`/`hook_infos` draws nothing on a
    // real session, which is what the chip did until it read these.
    const hook = (count: number): unknown => ({
      session_id: 'session_0',
      type: 'system',
      subtype: 'stop_hook_summary',
      hookCount: count,
      hookInfos: [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }],
      hookErrors: [],
      hookAdditionalContext: [],
      preventedContinuation: false,
      stopReason: '',
      hasOutput: true,
      level: 'suggestion',
      toolUseID: '00afa158-9a64-4fae-9c85-4866c0399894',
      uuid: 'uuid_7',
    });

    const units = fold([call('read', 0), hook(1)]);
    expect(kinds(units), 'the chip follows the run it came after').toEqual(['leaves', 'hooks']);
    const chip = units[1];
    expect(chip?.kind === 'hooks' ? chip.actions : null).toBe(1);
    expect(
      chip?.kind === 'hooks' ? chip.infos : [],
      'the command and the duration the captured row carries',
    ).toEqual([{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }]);
    expect(fold([hook(0)]), 'a frame reporting none draws nothing').toHaveLength(0);
  });

  it('reads the errors a failed hook summary carries', () => {
    // The captured row from a session transcript, where the ralph-wiggum
    // plugin's directory was gone: one hook, one error. The wire's own keys,
    // with the transcript's bookkeeping (parentUuid, cwd, version and the
    // like) dropped and its `sessionId` spelled `session_id`, the name this
    // feed carries. The errors do not pair with the hook rows in the corpus -
    // most failing rows hold fewer errors than infos - so a fixture built to
    // look paired would hide the fact that decides the shape. The em-dash is
    // the captured string's own, escaped for the source gate.
    const error =
      'Failed to run: Plugin directory does not exist: /Users/vedhavyas/.claude/plugins/cache/claude-code-plugins/ralph-wiggum/1.0.0 (ralph-wiggum@claude-code-plugins \u{2014} run /plugin to reinstall)';
    const failed: Record<string, unknown> = {
      session_id: '3dc2afa8-fdd1-40ff-bc7d-ffaace19246a',
      type: 'system',
      subtype: 'stop_hook_summary',
      hookCount: 1,
      hookInfos: [{ command: '${CLAUDE_PLUGIN_ROOT}/hooks/stop-hook.sh', durationMs: 0 }],
      hookErrors: [error],
      hookAdditionalContext: [],
      preventedContinuation: false,
      stopReason: '',
      hasOutput: true,
      level: 'suggestion',
      toolUseID: '8bdfbd8a-a578-441d-97ff-4d8a2923e1e7',
      uuid: '225cae5c-f638-4b31-afb2-700b8303dc16',
    };

    const units = fold([failed]);
    const chip = units[0];
    expect(
      chip?.kind === 'hooks' ? chip.errors : [],
      'the error the captured row carries, as its own list beside the infos',
    ).toEqual([error]);

    const clean = fold([{ ...failed, hookErrors: [] }]);
    expect(
      clean[0]?.kind === 'hooks' ? clean[0].errors : null,
      'and a summary that carries none reads as none',
    ).toEqual([]);
  });

  it("draws a hook's own row, carrying what the frames that reported it sent", () => {
    const units = fold(hookFrames());

    expect(kinds(units), 'one row for the run, rather than one for each frame').toEqual(['leaves']);
    const run = runOf(units);
    expect(run?.name, 'the hook the CLI matched, under its own name').toBe('SessionStart:startup');
    expect(run?.body, 'with the output it settled on').toBe('<redacted-hook-body>');
    // The response SUPERSEDES what the progress frames had printed rather than
    // being appended to it: the frames' output is cumulative, so a fold that
    // joined them would draw the same lines twice.
    expect(run?.body, 'and not the interim output the response repeats').not.toContain(
      'capture-line-2',
    );
    expect(run?.event, 'and the event said once, not twice').toBeNull();
  });

  it('draws a hook that has not settled as running, and settles that same row', () => {
    const frames = hookFrames();
    const running = fold(frames.slice(0, 3));

    expect(kinds(running), 'the row is drawn while the hook still runs').toEqual(['leaves']);
    expect(runOf(running)?.body, 'with the output it has printed so far').toContain(
      'capture-line-2',
    );

    expect(fold(frames), 'the response settles that row rather than opening a second').toHaveLength(
      1,
    );
  });

  it('marks a hook that exited non-zero', () => {
    const [started, , , response] = hookFrames();
    const failed = fold([
      started,
      { ...(response as Record<string, unknown>), exit_code: 2, outcome: 'error' },
    ]);

    expect(runOf(failed)?.failed, 'the mark a failure the CLI reported draws').toBe(true);
    // The control the mark needs: the same frames without the failure draw a
    // line, so a fold that marked every hook would not read as one that marks
    // the failed ones.
    expect(
      runOf(fold(hookFrames()))?.failed,
      'and a hook that exited clean stays a line rather than a failure',
    ).toBe(false);
  });

  it('says which event a hook ran for when its name does not already say it', () => {
    // Every captured run names its hook `<Event>:<matcher>`, so the event rides
    // the line inside the name. This is the guard for a name that carries none:
    // a row that could not say what kind of hook ran would be drawing less than
    // the frame sent.
    const [started] = hookFrames();
    const renamed = fold([{ ...(started as Record<string, unknown>), hook_name: 'notify.sh' }]);

    expect(runOf(renamed)?.name, 'the name the frame sent').toBe('notify.sh');
    expect(runOf(renamed)?.event, 'and the event beside it').toBe('SessionStart');
  });

  it('settles each call with the result that answers it', () => {
    const units = fold([
      call('read', 0),
      heard([result('toolu_read_0', 'output')]),
      call('read', 1),
      heard([{ type: 'tool_result', tool_use_id: 'toolu_read_1', content: 'no', is_error: true }]),
    ]);

    const calls = callsOf(units[0]).map((call) => call.leaf);
    expect(calls.map((leaf) => leaf.status)).toEqual(['completed', 'failed']);
  });

  it('draws a prompt that landed mid-turn as a turn of the reader', () => {
    // What the transcript read hoists out of an `attachment` row
    // (`{"type":"attachment","attachment":{"type":"queued_command","prompt":…,
    // "commandMode":"prompt"}}`): the shape a mid-turn prompt reaches this
    // page as, since the CLI never echoes one on stream-json.
    const queued = heard([
      {
        type: 'queued_command',
        prompt: 'The lead charter.md, does it get included as part of the Rust binary itself?',
        commandMode: 'prompt',
      },
    ]);

    const units = fold([queued]);
    expect(kinds(units)).toEqual(['user']);
    expect(units[0]?.kind === 'user' ? units[0].text : '').toBe(
      'The lead charter.md, does it get included as part of the Rust binary itself?',
    );

    // A prompt that carried more than words draws its words, and every other
    // block as the placeholder the server's own fold gives it.
    const multi = fold([
      heard([
        {
          type: 'queued_command',
          commandMode: 'prompt',
          prompt: [
            { type: 'text', text: 'What is wrong with this layout?' },
            { type: 'image', source: { media_type: 'image/png' } },
          ],
        },
      ]),
    ]);
    expect(multi[0]?.kind === 'user' ? multi[0].text : '').toBe(
      'What is wrong with this layout?\n[image]',
    );
  });

  it('draws a queued peer envelope as the peer card it is', () => {
    // 4,345 of this machine's queued prompts open with the envelope's own
    // bracket: a queued prompt is often somebody else's words arriving, and
    // drawn as the reader's own turn it is an accent bubble attributed to
    // them. The header is the producer's, from `peers/types.rs`.
    const queued = heard([
      {
        type: 'queued_command',
        commandMode: 'prompt',
        prompt:
          "[Message id=t-a399a7fb from agent 'lead' (org 'Personal')]\n\nProceed with the shape as described.",
      },
    ]);

    const units = fold([queued]);
    expect(kinds(units), 'not a turn of the reader').toEqual(['leaves']);
    const first = units[0];
    expect(cardsOf(first)[0]?.peer).toBe('lead');
  });

  it('keeps the completion notice the harness sends out of the conversation', () => {
    // The harness's background-completion report is not something a person
    // said, and the terminal drops the kind outright. **Two signals, and each
    // is pinned alone**: across 10,148 queued blocks in this machine's
    // transcripts, 3,113 carry the mode, 3,113 open with the tag, and none
    // disagrees - so the pair looks redundant and a row carrying both could
    // not tell a two-signal guard from a one-signal one.
    const byMode = heard([
      { type: 'queued_command', commandMode: 'task-notification', prompt: 'Task bj5g0t2kq done' },
    ]);
    const byTag = heard([
      {
        type: 'queued_command',
        commandMode: 'prompt',
        prompt: '<task-notification>Task bj5g0t2kq completed</task-notification>',
      },
    ]);

    expect(fold([byMode]), 'the mode alone drops it').toHaveLength(0);
    expect(fold([byTag]), 'and so does the tag alone').toHaveLength(0);
  });

  it('reads the image in a tool result the way the wire nests it', () => {
    // A result whose content is one image block, the shape this machine's own
    // transcripts carry: both the mime and the bytes sit under `source`, so
    // read off the block itself a tool's image draws with no mime at all.
    const drew = said([use('toolu_img', 'Read', { file_path: 'shot.png' })]);
    const answered = heard([
      {
        type: 'tool_result',
        tool_use_id: 'toolu_img',
        content: [
          { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AAAA' } },
        ],
      },
    ]);

    const [group] = fold([drew, answered]);
    const leaf = callsOf(group)[0]?.leaf;
    expect(leaf?.body, 'the image, under the name the wire gives it').toEqual([
      { kind: 'image', mime: 'image/png', uri: null },
    ]);
  });

  it('gives an attachment to the first turn of the frame, and not to both', () => {
    // The wire puts the file between two turns of words, and the file belongs
    // to the turn the frame opened: a second turn carrying it again draws the
    // same attachment twice under one message.
    const twice = heard([
      { type: 'text', text: 'first' },
      { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AAAA' } },
      { type: 'text', text: 'second' },
    ]);

    const units = fold([twice]);
    expect(kinds(units)).toEqual(['user', 'user']);
    expect(
      units[0]?.kind === 'user' ? units[0].files.length : 0,
      'the turn the file came with carries it',
    ).toBe(1);
    expect(units[1]?.kind === 'user' ? units[1].files : [], 'and the next one does not').toEqual(
      [],
    );
  });

  it('states no size for an attachment the wire gave as a url', () => {
    // A source the wire sends as a link carries no payload, so there is no
    // size to state - and the row draws the name it has rather than a dash.
    const linked = fold([
      heard([
        {
          type: 'image',
          source: { type: 'url', media_type: 'image/png', url: 'https://example.test/a.png' },
        },
      ]),
    ]);

    expect(linked[0]?.kind === 'user' ? linked[0].files : []).toEqual([
      { kind: 'image', mime: 'image/png', bytes: null },
    ]);
  });

  it('draws what a user turn attached', () => {
    // The live capture's own frame (user_message_blocks.jsonl, the scenario
    // that sends a one-pixel PNG with its words): a user turn's attachment has
    // no inbound route, so this is the shape it reaches the page as.
    const attached = heard([
      { type: 'text', text: 'Reply with the single word DONE.' },
      {
        type: 'image',
        source: {
          type: 'base64',
          media_type: 'image/png',
          data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=',
        },
      },
    ]);

    const [unit] = fold([attached]);
    expect(unit?.kind).toBe('user');
    expect(unit?.kind === 'user' ? unit.text : '').toBe('Reply with the single word DONE.');
    expect(unit?.kind === 'user' ? unit.files : []).toEqual([
      // 92 characters of base64 carry 68 bytes, and the row says the bytes.
      { kind: 'image', mime: 'image/png', bytes: 68 },
    ]);

    // A document, with no words beside it: the same arm, and a turn of the
    // reader's own rather than a row dropped. Its payload is redacted in the
    // capture this shape comes from, so only the name is asserted.
    const pdf = fold([
      heard([
        {
          type: 'document',
          source: { type: 'base64', media_type: 'application/pdf', data: '<redacted>' },
        },
      ]),
    ]);
    expect(kinds(pdf), 'a document with no words is a turn of its own').toEqual(['user']);
    expect(pdf[0]?.kind === 'user' ? pdf[0].files.map((file) => file.mime) : []).toEqual([
      'application/pdf',
    ]);
  });

  it('draws the fatal error the CLI sends, and finalizes the call it left open', () => {
    // No baseline at 2.1.280 carries a `{"type":"error"}` frame, so the shape
    // here is the one the decoder's own dispatch and `forge-sdk`'s decode test
    // both carry - `{"type":"error","error":<string>}` - and the string is the
    // representative one the mockup draws, not a capture. Only the words
    // inside it are unverified.
    const running = said([
      { type: 'tool_use', id: 'toolu_01GhIjKl', name: 'Bash', input: { command: 'git pull' } },
    ]);
    const fatal = {
      type: 'error',
      error: 'socket connection closed unexpectedly while reading stream-json',
    };

    const [group, notice] = fold([running, fatal]);
    const call = callsOf(group)[0]?.leaf;
    expect(call?.status, 'the turn is over, so the call it held is not still out').toBe('failed');
    expect(notice?.kind === 'notice' ? notice.notice.severity : null).toBe('error');
    expect(notice?.kind === 'notice' ? notice.notice.text : '').toBe(
      'socket connection closed unexpectedly while reading stream-json',
    );

    // The guard drops both of these, and each for its own reason: one carries a
    // blank string and one carries no string at all. Both are still the end of
    // the turn, so the call draws failed either way.
    for (const [said, why] of [
      [{ type: 'error', error: '   ' }, 'a blank string'],
      [{ type: 'error', error: null }, 'a value that is not a string'],
    ] as const) {
      const units = fold([running, said]);
      expect(kinds(units), `no notice for ${why}`).toEqual(['leaves']);
      const held = callsOf(units[0])[0]?.leaf;
      expect(held?.status, `and the turn still ended: ${why}`).toBe('failed');
    }

    // A dispatched agent's failure is not this turn's: its frames are the
    // SUBAGENTS surface's, and the drawing loop skips them, so a pre-pass that
    // read one would finalize a call the page never drew a row for.
    const child = { ...fatal, parent_tool_use_id: 'toolu_dispatch' };
    const units = fold([running, child]);
    const held = callsOf(units[0])[0]?.leaf;
    expect(held?.status, 'a sub-agent failing says nothing about this turn').toBe('pending');
    expect(kinds(units), 'and draws no failure line here').toEqual(['leaves']);
  });

  it('sweeps only the calls that were open when the turn failed', () => {
    // The live path accumulates every frame since the last page into ONE turn,
    // so a sweep that took the whole turn would mark calls the CLI opened
    // AFTERWARDS as failed while they are still running: interrupt a turn,
    // prompt again, and the new turn's first call draws red until its own
    // result lands.
    const before = said([
      { type: 'tool_use', id: 'toolu_before', name: 'Bash', input: { command: 'just check' } },
    ]);
    const ended = { type: 'result', is_error: true, subtype: 'error_during_execution' };
    const after = said([
      { type: 'tool_use', id: 'toolu_after', name: 'Read', input: { file_path: 'a.rs' } },
    ]);

    const units = fold([before, ended, after]);
    const calls = units.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);

    expect(calls.map((leaf) => leaf.id)).toEqual(['toolu_before', 'toolu_after']);
    expect(calls[0]?.status, 'the call the turn ended on is abandoned').toBe('failed');
    expect(calls[1]?.status, 'and the one it started afterwards is still out').toBe('pending');
  });

  /**
   * #1836: a restart kills the CLI mid-call, the resumed history keeps the
   * call's `tool_use` with no result and no failing frame, and a fold that
   * reads only boundaries leaves the row spinning forever. The terminal
   * settles exactly these failed on its resume
   * (`finalize_turn_runtime_artifacts(Failed)`), and the caller's `ended`
   * says the same: this turn's history is closed, so its unanswered call is
   * never coming back.
   */
  it('settles the call a closed turn never answered, backgrounded or not', () => {
    const load = said([use('tu_never', 'Skill', { skill: 'slow-skill' })]);

    const [live] = fold([load]);
    expect(
      callsOf(live).map((call) => call.leaf.status),
      'a turn still being written waits for the answer',
    ).toEqual(['pending']);

    const [settled] = fold([load], null, false, true);
    expect(
      callsOf(settled).map((call) => call.leaf.status),
      'the turn ended without an answer: the call draws failed, not spinning',
    ).toEqual(['failed']);

    // A backgrounded call whose launch never landed is the same spinner one
    // call type over: the terminal clears its background roster before the
    // resume's sweep, so an input-keyed exemption would leave it standing.
    const launched = said([
      {
        type: 'tool_use',
        id: 'tu_bg',
        name: 'Bash',
        input: { command: 'sleep 30', run_in_background: true },
      },
    ]);
    const [bg] = fold([launched], null, false, true);
    expect(
      callsOf(bg).map((call) => call.leaf.status),
      'a launch with no task frames fails with the turn',
    ).toEqual(['failed']);

    // What DOES hold one open is the task's own fact: a start the wire
    // carried exempts the call, input or no input.
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'tu_bg',
      is_backgrounded: true,
    };
    const [held] = fold([launched, started], null, false, true);
    expect(
      callsOf(held).map((call) => call.leaf.status),
      'a live task holds its call open past the turn',
    ).toEqual(['in_progress']);
  });

  /**
   * The FRAME-BUILT carrier is the other half of the settle: frames arrive on
   * a live page, whose `live` stays true for good there (#1486), so `ended`
   * never fires for it - the RESULT frame inside the fold is what closes the
   * turn, and a call the frames never answered before it settles failed.
   */
  it('settles an unanswered call when the fold own frames carried the end', () => {
    const load = said([use('tu_frame', 'Skill', { skill: 'slow-skill' })]);
    const ended = { type: 'result', is_error: false, subtype: 'success' };

    const [framed] = fold([load, ended], null, true, false);
    expect(
      callsOf(framed).map((call) => call.leaf.status),
      'the result frame closed the turn: the call before it draws failed',
    ).toEqual(['failed']);

    const [open] = fold([load], null, true, false);
    expect(
      callsOf(open).map((call) => call.leaf.status),
      'and with no end in the frames it still waits',
    ).toEqual(['pending']);
  });

  it('sweeps the calls of every failure in the turn, not only the first', () => {
    // Two failing turns in one fold - which the live path reaches when no page
    // lands between them - settle BOTH turns' calls. First-wins sweeps the
    // first and leaves the second's calls pending, so the roll-up says work is
    // still going, which is the complaint #1323 is filed about, drawn again.
    const first = said([
      { type: 'tool_use', id: 'toolu_a', name: 'Bash', input: { command: 'just check' } },
    ]);
    const failed = { type: 'result', is_error: true, subtype: 'error_during_execution' };
    const second = said([
      { type: 'tool_use', id: 'toolu_b', name: 'Bash', input: { command: 'just check' } },
    ]);

    const units = fold([first, failed, second, failed]);
    const calls = units.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);

    expect(
      calls.map((leaf) => leaf.status),
      'both turns abandoned their calls',
    ).toEqual(['failed', 'failed']);
  });

  it('says a failed turn failed, and finalizes the call it left open', () => {
    // The result frame is the interrupt capture's own (interrupt.jsonl) for
    // every field the turn's verdict rides on - its subtype, `is_error`, its
    // clock, its `terminal_reason` and its errors - and the usage block is the
    // mockup's, because the capture's is all zeroes and this row has to draw
    // figures. Its prompt carries no tool call either: the calls here are what
    // an interrupt catches, which is the shape the capture shows rather than
    // one it carries.
    //
    // **And an answered call rides beside the open one**, because that is the
    // ordinary interrupt: a turn that finished three calls and was killed in
    // the fourth. A fold that failed every call in a failed turn would paint
    // all four red, and only this second call can tell it from the right one.
    const answered = said([
      { type: 'tool_use', id: 'toolu_01AbCdEf', name: 'Read', input: { file_path: 'a.rs' } },
    ]);
    const itsResult = heard([
      { type: 'tool_result', tool_use_id: 'toolu_01AbCdEf', content: 'ok', is_error: false },
    ]);
    const running = said([
      { type: 'tool_use', id: 'toolu_01GhIjKl', name: 'Bash', input: { command: 'just check' } },
    ]);
    const interrupted = heard([text('[Request interrupted by user]')]);
    const ended = {
      type: 'result',
      is_error: true,
      subtype: 'error_during_execution',
      errors: ['[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null'],
      terminal_reason: 'aborted_streaming',
      duration_ms: 801,
      duration_api_ms: 0,
      usage: { input_tokens: 4200, output_tokens: 1100 },
    };

    const units = fold([answered, itsResult, running, interrupted, ended]);
    const [group, , report, notice] = units;
    const calls = callsOf(group).map((call) => call.leaf);
    expect(
      calls.map((leaf) => leaf.status),
      'only the call left open failed',
    ).toEqual(['completed', 'failed']);
    expect(report?.kind, 'the report row still reports what the turn spent').toBe('report');
    expect(report?.kind === 'report' ? report.info.failed : null, 'and marks it failed').toBe(true);
    expect(notice?.kind === 'notice' ? notice.notice.severity : null).toBe('error');
    expect(notice?.kind === 'notice' ? notice.notice.text : '').toBe(
      'Turn failed: error_during_execution \u{b7} aborted_streaming\n' +
        '[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null',
    );
  });

  it('draws a turn that hit its cap without repeating the reason it gives', () => {
    // exit_plan_mode.jsonl's own ending: the subtype already says `max_turns`.
    const capped = {
      type: 'result',
      is_error: true,
      subtype: 'error_max_turns',
      errors: ['Reached maximum number of turns (3)'],
      terminal_reason: 'max_turns',
      duration_ms: 1_084_000,
    };

    const [, report, notice] = fold([said([text('Nine calls in, the cap lands.')]), capped]);
    expect(report?.kind).toBe('report');
    expect(notice?.kind === 'notice' ? notice.notice.text : '').toBe(
      'Turn failed: error_max_turns\nReached maximum number of turns (3)',
    );
  });

  it('ends a backgrounded call on the update alone, on the call it belongs to', () => {
    // **The frame the commit names as the ending, alone.** Every live capture
    // follows a `task_updated` with a `task_notification` carrying the same
    // tool_use_id, so a fold that resolved only the notification would pass
    // every other test here - and a task ending on a bare update would draw as
    // running forever. `task_started` is the only frame that names the call a
    // task id belongs to, which is what the update resolves through.
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_01Bg',
        name: 'Bash',
        input: { command: 'sleep 30', run_in_background: true },
      },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_01Bg',
      is_backgrounded: true,
    };
    const ended = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'bj5g0t2kq',
      patch: { status: 'completed' },
    };

    const [group] = fold([launch, started, ended]);
    const calls = callsOf(group).map((call) => call.leaf);
    expect(calls, 'one call, patched rather than joined by a second row').toHaveLength(1);
    expect(calls[0]?.id, 'and the call the launch opened').toBe('toolu_01Bg');
    expect(calls[0]?.status, 'the update alone is the ending').toBe('completed');

    // A word the page does not know leaves the call where it was rather than
    // guessing: `completed` is the guess that would look right and be wrong.
    const odd = fold([
      launch,
      started,
      { type: 'system', subtype: 'task_updated', task_id: 'bj5g0t2kq', patch: { status: '?' } },
    ]);
    const held = callsOf(odd[0]).map((call) => call.leaf);
    expect(held[0]?.status, 'an unknown word changes nothing').toBe('in_progress');

    // And an update whose own `task_started` was never seen is dropped rather
    // than guessed at: `task_updated` names only the task, so the wrong call
    // would be worse than none - which is the terminal's call on the same
    // frame. The control rides beside it: the same frames WITH the start do
    // end the call, so a fold that dropped every update could not pass.
    const orphan = fold([
      launch,
      {
        type: 'system',
        subtype: 'task_updated',
        task_id: 'bj5g0t2kq',
        patch: { status: 'killed' },
      },
    ]);
    const alone = callsOf(orphan[0]).map((call) => call.leaf);
    expect(alone[0]?.status, 'an unplaced update leaves the call where it was').toBe('pending');
  });

  it('ends a backgrounded call from the notice a transcript carries instead', () => {
    // **The page read's only ending.** A transcript holds no task frames at
    // all - zero across every one on this machine - and persists the ending as
    // a `<task-notification>` text block instead, so a page that read only the
    // frames would draw the call as finished at launch every time it re-reads.
    // The block's own fields are the frame's: id, status and summary.
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_01ED98Nzdco6KEJMBXvs1EKN',
        name: 'Bash',
        input: { command: 'sleep 30', run_in_background: true },
      },
    ]);
    const notice = heard([
      {
        type: 'queued_command',
        commandMode: 'task-notification',
        prompt:
          '<task-notification>\n<task-id>banr9rj33</task-id>\n' +
          '<tool-use-id>toolu_01ED98Nzdco6KEJMBXvs1EKN</tool-use-id>\n' +
          '<status>stopped</status>\n<summary>Watch the docs workflow run</summary>\n</task-notification>',
      },
    ]);

    const [group] = fold([launch, notice]);
    const calls = callsOf(group).map((call) => call.leaf);
    expect(calls[0]?.status, 'a stopped task draws as the kill it is').toBe('killed');
    expect(calls[0]?.note, 'and carries what the notice said').toEqual({
      text: 'Watch the docs workflow run \u{b7} stopped',
      tone: 'fail',
    });

    // The tone follows the same word the status does: an unknown one gets no
    // tone at all, because red is a claim and so is green.
    const unknown = fold([
      launch,
      heard([
        {
          type: 'queued_command',
          commandMode: 'task-notification',
          prompt:
            '<task-notification>\n<tool-use-id>toolu_01ED98Nzdco6KEJMBXvs1EKN</tool-use-id>\n' +
            '<status>halfway</status>\n<summary>Watch the docs workflow run</summary>\n</task-notification>',
        },
      ]),
    ]);
    const odd = callsOf(unknown[0]).map((call) => call.leaf);
    expect(odd[0]?.note?.tone, 'a word it does not know is not a failure').toBeNull();
  });

  it('keeps the status a notice with no word for it does not state', () => {
    // The other arm of the same rule, and the only shape that reaches it: a
    // notice carrying a call id and no `<status>`. Every status-less notice on
    // this machine lacks the id and is dropped before here, so this is what
    // keeps the arm reachable - and what it must not do is walk the call back
    // to running, which the task frames have already said ended.
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_01St',
        name: 'Bash',
        input: { command: 'sleep 30', run_in_background: true },
      },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_01St',
      is_backgrounded: true,
    };
    const killed = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'bj5g0t2kq',
      patch: { status: 'killed' },
    };
    const wordless = heard([
      {
        type: 'queued_command',
        commandMode: 'task-notification',
        prompt:
          '<task-notification>\n<tool-use-id>toolu_01St</tool-use-id>\n' +
          '<summary>Run slow counting loop</summary>\n</task-notification>',
      },
    ]);

    const units = fold([launch, started, killed, wordless]);
    const calls = units.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);

    expect(calls[0]?.status, 'the notice keeps the status the frames set').toBe('killed');
  });

  it('draws no line for a task the wire did not say outlives its turn', () => {
    // A foreground call's own result is already on the row, and a dispatched
    // agent's report is the row's own body: the harness's summary under either
    // one repeats what the row already says - the subagent's report twice, and
    // a foreground command's own title. Both shapes are in the captures.
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_01Fg',
        name: 'Bash',
        input: { command: 'sleep 1 && echo fg' },
      },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bf0y4w5hl',
      tool_use_id: 'toolu_01Fg',
      is_backgrounded: false,
    };
    const notified = {
      type: 'system',
      subtype: 'task_notification',
      task_id: 'bf0y4w5hl',
      tool_use_id: 'toolu_01Fg',
      status: 'completed',
      summary: 'Sleep 65 seconds then echo a marker',
    };

    const units = fold([launch, started, notified]);
    const calls = units.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);

    // The absent field is its own case, and the corpus carries it: a
    // `local_workflow` task's `task_started` names no `is_backgrounded` at all,
    // and its notification carries a summary - read as anything but "not said
    // to be backgrounded", that workflow draws the duplicate line the case
    // above exists to remove. (`legacy-surface` carries the same shape with
    // `false`.)
    const unstated = fold([launch, { ...started, is_backgrounded: undefined }, notified]);
    const silent = unstated.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);
    expect(silent[0]?.note, 'a task that does not say is not treated as backgrounded').toBeNull();

    expect(calls[0]?.status, 'the ending still settles the call').toBe('completed');
    expect(calls[0]?.note, 'and the row is not given a line it already says').toBeNull();
  });

  it('keeps two tasks in flight apart whatever order their endings arrive in', () => {
    // A backgrounded bash while a backgrounded agent runs is ordinary, and the
    // endings arrive in the order the tasks finish rather than the order they
    // started. The map from a task to its call is what keeps each ending with
    // its own row; a fold keyed on the call that started last hands every
    // ending to that one.
    const first = said([
      {
        type: 'tool_use',
        id: 'toolu_01',
        name: 'Bash',
        input: { command: 'sleep 30', run_in_background: true },
      },
    ]);
    const second = said([
      {
        type: 'tool_use',
        id: 'toolu_02',
        name: 'Bash',
        input: { command: 'sleep 60', run_in_background: true },
      },
    ]);
    const started = (task: string, call: string): unknown => ({
      type: 'system',
      subtype: 'task_started',
      task_id: task,
      tool_use_id: call,
      is_backgrounded: true,
    });
    const killedSecond = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'task-b',
      patch: { status: 'killed' },
    };
    const finishedFirst = {
      type: 'system',
      subtype: 'task_notification',
      task_id: 'task-a',
      tool_use_id: 'toolu_01',
      status: 'completed',
      summary: 'the first one finished',
    };

    const units = fold([
      first,
      started('task-a', 'toolu_01'),
      second,
      started('task-b', 'toolu_02'),
      killedSecond,
      finishedFirst,
    ]);
    const calls = units.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);
    expect(calls.map((leaf) => [leaf.id, leaf.status])).toEqual([
      ['toolu_01', 'completed'],
      ['toolu_02', 'killed'],
    ]);

    // An update naming a task this turn never saw started is dropped even with
    // a live sibling beside it: the wrong call would be worse than none.
    const orphan = fold([
      first,
      started('task-a', 'toolu_01'),
      second,
      started('task-b', 'toolu_02'),
      {
        type: 'system',
        subtype: 'task_updated',
        task_id: 'task-gone',
        patch: { status: 'killed' },
      },
    ]);
    const held = orphan.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);
    expect(
      held.map((leaf) => leaf.status),
      'both are still out',
    ).toEqual(['in_progress', 'in_progress']);
  });

  it('keeps a finished call finished when a later update is unreadable', () => {
    // The regression the `null` return from `taskStatus` exists to prevent: an
    // update whose status word this page does not know must leave the call
    // where it was, and walking a completed call back to running is the guess
    // that would look most right.
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_01Bg',
        name: 'Bash',
        input: { command: 'sleep 30', run_in_background: true },
      },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_01Bg',
      is_backgrounded: true,
    };
    const finished = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'bj5g0t2kq',
      patch: { status: 'completed' },
    };
    const unreadable = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'bj5g0t2kq',
      patch: { status: 'half-done' },
    };

    const units = fold([launch, started, finished, unreadable]);
    const calls = units.flatMap((unit) => callsOf(unit)).map((call) => call.leaf);

    expect(calls[0]?.status, 'the call stays where the readable frame put it').toBe('completed');
  });

  it('leaves a backgrounded call running after the result that started it', () => {
    // The launch and its result as the capture has them
    // (crates/forge-test-harness/baselines/sdk/2.1.280/backgrounded_bash_lifecycle.jsonl).
    // The result is a clean one that says the command STARTED, so a fold with
    // no task frames draws a running command as a finished one.
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
        name: 'Bash',
        input: {
          command: 'sleep 1 && echo forge-bash-bg-ok',
          description: 'Echo test string after brief sleep',
          run_in_background: true,
        },
      },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
      description: 'Echo test string after brief sleep',
      is_backgrounded: true,
      task_type: 'local_bash',
      uuid: '30b960f6-c50d-4594-bd15-2fddf70ee88e',
    };
    const launched = heard([
      {
        type: 'tool_result',
        tool_use_id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
        content: 'Command running in background with ID: bj5g0t2kq.',
        is_error: false,
      },
    ]);

    const [group] = fold([launch, launched, started]);
    const call = callsOf(group)[0]?.leaf;
    expect(call?.status, 'the launch result is not the end of a backgrounded call').toBe(
      'in_progress',
    );
  });

  it('settles a backgrounded call on the frame that ends it, with what that frame said', () => {
    const launch = said([
      {
        type: 'tool_use',
        id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
        name: 'Bash',
        input: { command: 'sleep 1 && echo forge-bash-bg-ok', run_in_background: true },
      },
    ]);
    const launched = heard([
      {
        type: 'tool_result',
        tool_use_id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
        content: 'Command running in background with ID: bj5g0t2kq.',
      },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
      // The capture's own field, and the one that makes the note worth a line.
      is_backgrounded: true,
    };
    const ended = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'bj5g0t2kq',
      patch: { status: 'completed', end_time: 1790162965845 },
    };
    const notified = {
      type: 'system',
      subtype: 'task_notification',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
      status: 'completed',
      summary: 'Background command "Echo test string after brief sleep" completed (exit code 0)',
    };

    const first = fold([launch, launched, started, ended, notified]);
    const done = callsOf(first[0])[0]?.leaf;
    expect(done?.status).toBe('completed');
    expect(done?.note, 'the harness sentence, drawn as it wrote it').toEqual({
      text: 'Background command "Echo test string after brief sleep" completed (exit code 0)',
      tone: 'sum',
    });

    // The killed end, as stop_task.jsonl carries it: the patch says `killed`,
    // the notification says `stopped`, and a summary that says neither is the
    // one the status word is drawn beside.
    const killed = {
      type: 'system',
      subtype: 'task_updated',
      task_id: 'bj5g0t2kq',
      patch: { status: 'killed', end_time: 1790162923382 },
    };
    const stopped = {
      type: 'system',
      subtype: 'task_notification',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_012ygCheCDa6s8YmU5JxxVp2',
      status: 'stopped',
      summary: 'Run slow counting loop',
    };

    const second = fold([launch, launched, started, killed, stopped]);
    const dead = callsOf(second[0])[0]?.leaf;
    expect(dead?.status, 'a stopped task draws as the kill it is').toBe('killed');
    expect(dead?.note).toEqual({ text: 'Run slow counting loop \u{b7} stopped', tone: 'fail' });
  });

  it('settles a foreground task from the turn, not from its own task frame', () => {
    // `task_started` arrives for every task the CLI runs, and the wire says
    // whether it outlives its turn. A foreground call does not, so its task
    // frame does not get to overrule the turn's verdict: a call the turn
    // abandoned draws failed, which is what the sweep is for. Letting the task
    // status win draws it running forever, which is the complaint the interrupt
    // issue is filed about, drawn again.
    const launch = said([
      { type: 'tool_use', id: 'toolu_fg', name: 'Bash', input: { command: 'just check' } },
    ]);
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bfg0001',
      tool_use_id: 'toolu_fg',
      is_backgrounded: false,
    };
    const ended = { type: 'result', is_error: true, subtype: 'error_during_execution' };

    const interrupted = fold([launch, started, ended]);
    const row = callsOf(interrupted[0])[0]?.leaf;
    expect(row?.status, 'an abandoned foreground call draws failed, not running').toBe('failed');

    // The flag-less shape the corpus carries (`workflow.jsonl`), whose task does
    // not say it outlives the turn either, and whose failing result still
    // settles the row.
    const unnamed = { ...started, is_backgrounded: undefined };
    const answered = heard([
      { type: 'tool_result', tool_use_id: 'toolu_fg', content: 'exit 1', is_error: true },
    ]);

    const settled = fold([launch, unnamed, answered]);
    const second = callsOf(settled[0])[0]?.leaf;
    expect(second?.status, 'and a failing result still settles it').toBe('failed');
  });

  it("reports the clock the turn's own last row carried", () => {
    // The result frame carries no instant, and a turn read from a transcript
    // has no result frame at all - so the only clock a settled row can report
    // is the one its own rows wrote. Without it the row's first fact is a
    // permanent dash.
    const at = (stamp: string, body: unknown): unknown => ({
      type: 'assistant',
      uuid: `a-${stamp}`,
      timestamp: stamp,
      message: { id: `m-${stamp}`, role: 'assistant', model: 'claude-opus-5', content: [body] },
    });

    const units = fold([
      at('2026-09-29T10:00:00.000Z', text('first')),
      at('2026-09-29T10:00:04.000Z', text('second')),
      {
        type: 'result',
        uuid: 'r1',
        duration_ms: 4000,
        duration_api_ms: 3000,
        usage: { input_tokens: 10, output_tokens: 2 },
      },
    ]);

    // The KINDS, rather than finding the report among them: this frame carries
    // no error, so a fold that drew the failure line for a turn that finished
    // would add a notice here and a search for the report would never see it.
    expect(kinds(units), 'no failure line for a turn that did not fail').toEqual([
      'text',
      'text',
      'report',
    ]);
    const report = units.find((unit) => unit.kind === 'report');
    expect(report?.kind === 'report' ? report.info.ended_at_utc : null).toBe(
      '2026-09-29T10:00:04.000Z',
    );
    expect(report?.kind === 'report' ? report.info.failed : null, 'and the row is not marked').toBe(
      false,
    );
  });

  it('sums the thinking deltas a turn carried', () => {
    // The wire sends the counter as a subtype of its own, and it restarts at
    // every thinking block - so a turn's estimate is the sum of the deltas,
    // and reading the absolute field understates any turn that thought twice.
    const thought = (delta: number): unknown => ({
      type: 'system',
      subtype: 'thinking_tokens',
      estimated_tokens_delta: delta,
      uuid: `think-${delta}`,
    });
    const result = {
      type: 'result',
      uuid: 'r1',
      duration_ms: 1000,
      duration_api_ms: 900,
      usage: { input_tokens: 10, output_tokens: 2 },
    };

    const units = fold([thought(161), thought(50), thought(299), result]);
    const report = units.find((unit) => unit.kind === 'report');
    expect(
      report?.kind === 'report' ? report.info.thinking_tokens : null,
      'every block the turn thought, not the last one',
    ).toBe(510);
  });

  it('draws a prompt as the turn the reader wrote, and a result as no turn at all', () => {
    const units = fold([
      heard([text('run the gate')]),
      call('bash', 0),
      heard([result('toolu_bash_0', 'all green')]),
    ]);

    expect(kinds(units)).toEqual(['user', 'leaves']);
    expect(units[0]?.kind === 'user' ? units[0].text : null).toBe('run the gate');
  });
});

describe("the CLI's retry line", () => {
  /** One `api_retry` frame, as the wire shapes it. */
  const retry = (fields: Record<string, unknown>): unknown => ({
    type: 'system',
    subtype: 'api_retry',
    attempt: 2,
    max_retries: 4,
    retry_delay_ms: 1500,
    error_status: 529,
    error: 'server_error',
    uuid: 'r-retry',
    ...fields,
  });

  const noticed = (units: Unit[]) => units.filter((unit) => unit.kind === 'notice');

  it('draws one warning line for a retry, with the attempt and the delay', () => {
    const units = fold([said([text('working')]), retry({})]);

    const notices = noticed(units);
    expect(notices, 'one line, not one per frame').toHaveLength(1);
    const notice = notices[0]?.kind === 'notice' ? notices[0].notice : null;
    expect(notice?.severity, 'a retry is a warning').toBe('warning');
    expect(notice?.text, "the terminal's own words, so the two views agree").toBe(
      'API retry after server_error HTTP 529',
    );
    expect(notice?.chip, 'which attempt of how many').toBe('attempt 2 / 4');
    expect(notice?.sub, 'and how long it waits').toBe('retrying in 1.5s');
  });

  it('rewrites its own line as the attempts advance, rather than stacking them', () => {
    // A storm is ONE row saying why, not fifty: a later frame replaces the
    // run's line the way the terminal's deduped turn notice does.
    const units = fold([
      retry({ attempt: 1, retry_delay_ms: 4200, error: 'rate_limit', error_status: 429 }),
      retry({ attempt: 2, retry_delay_ms: 8700, error: 'rate_limit', error_status: 429 }),
      retry({ attempt: 3, retry_delay_ms: 16200, error: 'rate_limit', error_status: 429 }),
    ]);

    const notices = noticed(units);
    expect(notices, 'one line for the whole run').toHaveLength(1);
    const notice = notices[0]?.kind === 'notice' ? notices[0].notice : null;
    expect(notice?.chip, 'carrying the latest attempt').toBe('attempt 3 / 4');
    expect(notice?.sub, 'and the latest delay').toBe('retrying in 16.2s');
  });

  it('names the unknown classification and omits a status the wire did not carry', () => {
    const units = fold([
      retry({ error: 'something_new', error_status: undefined, retry_delay_ms: 250 }),
    ]);

    const notice = noticed(units)[0];
    expect(
      notice?.kind === 'notice' ? notice.notice.text : null,
      'no HTTP where none arrived',
    ).toBe('API retry after connection error');
    expect(
      notice?.kind === 'notice' ? notice.notice.sub : null,
      'milliseconds read as themselves',
    ).toBe('retrying in 250ms');
  });
});

describe('the rate-limit windows', () => {
  /**
   * One `rate_limit_event` frame, as the wire shapes it. The reset sits long
   * past, so the countdown those words end on is "now" forever - the pins
   * below stay exact without a faked clock.
   */
  const windowed = (info: Record<string, unknown>): unknown => ({
    type: 'rate_limit_event',
    rate_limit_info: { resetsAt: 1_741_280_000, rateLimitType: 'five_hour', ...info },
    uuid: 'r-limit',
    session_id: 's-1',
  });

  const noticed = (units: Unit[]) => units.filter((unit) => unit.kind === 'notice');

  it("draws a window closing as a warning line, in the terminal's words", () => {
    const units = fold([windowed({ status: 'allowed_warning', utilization: 0.91 })]);

    const notices = noticed(units);
    expect(notices, 'one line for the window').toHaveLength(1);
    const notice = notices[0]?.kind === 'notice' ? notices[0].notice : null;
    expect(notice?.severity, 'a closing window is a warning').toBe('warning');
    expect(notice?.text, "the terminal's own words, so the two views agree").toBe(
      "Approaching rate limit, you've used 91% of your 5-hour rate limit. Resets in now at 16:53 UTC.",
    );
  });

  it('upgrades the same window to an error in place, rather than stacking', () => {
    // The key is the window, not the status: a window that escalates from
    // warning to rejected rewrites the line it already drew.
    const units = fold([
      windowed({ status: 'allowed_warning' }),
      windowed({ status: 'rejected', utilization: 0.99 }),
    ]);

    const notices = noticed(units);
    expect(notices, 'one line for the whole window').toHaveLength(1);
    const notice = notices[0]?.kind === 'notice' ? notices[0].notice : null;
    expect(notice?.severity, 'a rejected window is an error').toBe('error');
    expect(notice?.text).toBe(
      "Rate limit reached, you've used 99% of your 5-hour rate limit. Resets in now at 16:53 UTC.",
    );
  });

  it('keeps the loudest line when the same window walks back', () => {
    // The terminal's no-downgrade guard (`upsert_turn_notice` refuses a lower
    // stage): a window that flips from rejected back to a warning keeps the
    // line it already drew, where a fresh warning would soften "Rate limit
    // reached" to "Approaching".
    const units = fold([
      windowed({ status: 'rejected', utilization: 0.99 }),
      windowed({ status: 'allowed_warning', utilization: 0.8 }),
    ]);

    const notices = noticed(units);
    expect(notices, 'one line for the whole window').toHaveLength(1);
    const notice = notices[0]?.kind === 'notice' ? notices[0].notice : null;
    expect(notice?.severity, 'the loudest stage holds').toBe('error');
    expect(notice?.text).toBe(
      "Rate limit reached, you've used 99% of your 5-hour rate limit. Resets in now at 16:53 UTC.",
    );
  });

  it('opens a fresh line when the window resets', () => {
    const units = fold([
      windowed({ status: 'rejected' }),
      windowed({ status: 'allowed_warning', resetsAt: 1_741_280_000 + 18_000 }),
    ]);

    expect(noticed(units), 'a new window is a new incident').toHaveLength(2);
  });

  it('draws nothing for a window that is allowed or unknown to this build', () => {
    // The terminal routes both to no notice, and this fold matches rather
    // than draws a line the other view does not have.
    const units = fold([
      said([text('working')]),
      windowed({ status: 'allowed' }),
      windowed({ status: 'something_new' }),
    ]);

    expect(noticed(units)).toHaveLength(0);
    expect(kinds(units)).toEqual(['text']);
  });
});
