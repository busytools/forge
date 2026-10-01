import { describe, expect, it } from 'vitest';

import { fold, type Unit } from './units';

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

describe('one turn folded into the units a view draws', () => {
  it('folds consecutive calls into one group', () => {
    // The rule the mockup was built on and the one a reader gets wrong first:
    // a run of calls between two prompts is ONE group, not one row each.
    const units = fold([call('read', 0), call('search', 1), call('read', 2)]);

    expect(units).toHaveLength(1);
    const [group] = units;
    expect(group?.kind).toBe('group');
    expect(group?.kind === 'group' ? group.families.map((f) => f.label) : []).toEqual([
      'read',
      'search',
    ]);
    expect(group?.kind === 'group' ? group.families[0]?.calls.length : 0).toBe(2);
  });

  it('folds a mutation into the run as its own family', () => {
    const units = fold([call('read', 0), call('edit', 1), call('read', 2)]);

    expect(units, 'the mutation does not break the run').toHaveLength(1);
    const [group] = units;
    const labels = group?.kind === 'group' ? group.families.map((f) => f.label) : [];
    expect(labels).toEqual(['read', 'edit']);
  });

  it('splits the run on the calls the mockup draws alone', () => {
    const question = said([use('toolu_q', 'AskUserQuestion', { questions: [] })]);
    const peer = said([
      use('toolu_p', 'mcp__forge__agents__tell', { project: 'x', message: 'hi' }),
    ]);

    for (const breaker of [question, peer]) {
      const units = fold([call('read', 0), breaker, call('read', 1)]);
      expect(units, 'the run splits around a call drawn on its own').toHaveLength(3);
      expect(kinds(units)[1], 'and that call is the unit in the middle').toMatch(
        /question|messages/,
      );
    }
  });

  it('draws what the model thought, which the wire carries and nothing drew', () => {
    // The body is on the wire - `ContentBlock::Thinking { thinking, signature }`
    // in primitives, and a real transcript holds one - and this fold had no arm
    // for it, so the block fell through and the words were dropped. An empty
    // thinking draws nothing, the way the terminal skips one.
    const thought = said([{ type: 'thinking', thinking: 'the model wondered', signature: 'sig' }]);
    const empty = said([{ type: 'thinking', thinking: '', signature: 'sig' }]);

    const units = fold([thought]);
    expect(kinds(units), 'the thinking is a row rather than a drop').toEqual(['thinking']);
    expect(units[0]?.kind === 'thinking' ? units[0].text : '').toBe('the model wondered');
    expect(kinds(fold([empty])), 'and an empty one is not a row').toEqual([]);
  });

  it('keeps a run whole across a thinking row', () => {
    // A thinking block is commentary ON the work rather than a separator
    // between pieces of it: the terminal has no thinking variant at all, so its
    // run cannot break on one. The regression was measured on a real turn:
    // drawing each thought as its own unit split one run of 24 calls into
    // twelve groups, where the same turn drew four.
    const units = fold([
      call('read', 0),
      said([{ type: 'thinking', thinking: 'about the file', signature: 's' }]),
      call('read', 1),
    ]);

    const groups = units.filter((unit) => unit.kind === 'group');
    expect(kinds(units), 'the row draws above the run it interrupted').toEqual([
      'thinking',
      'group',
    ]);
    expect(
      units.filter((u) => u.kind === 'thinking'),
      'the words still draw',
    ).toHaveLength(1);
    expect(groups, 'one run, not two').toHaveLength(1);
    expect(groups[0]?.kind === 'group' ? groups[0].families[0]?.calls.length : 0).toBe(2);
  });

  it('keeps a message batch whole across a thinking row', () => {
    // The class list calls a run of peer messages a tool run, so the same rule
    // holds: a thought between two messages is commentary, not a separator. The
    // round measured thirteen of ninety-four real turns changing by exactly
    // this unit when the batch split.
    const tell = (n: number): unknown =>
      said([
        use(`toolu_tell_${n}`, 'mcp__forge__agents__tell', { project: 'x', message: `m${n}` }),
      ]);

    const units = fold([
      tell(1),
      said([{ type: 'thinking', thinking: 'between the messages', signature: 's' }]),
      tell(2),
    ]);

    const batches = units.filter((unit) => unit.kind === 'messages');
    expect(kinds(units), 'the row draws above the whole batch').toEqual(['thinking', 'messages']);
    expect(batches, 'one batch, not two').toHaveLength(1);
    expect(batches[0]?.key, 'named by the first message, which is data the turn cannot move').toBe(
      'p-toolu_tell_1',
    );
    const cards =
      batches[0]?.kind === 'messages' ? batches[0].lanes.flatMap((lane) => lane.cards) : [];
    expect(cards, 'with both messages in it').toHaveLength(2);
    expect(
      units.filter((u) => u.kind === 'thinking'),
      'the words still draw',
    ).toHaveLength(1);
  });

  it('breaks the run on anything that is not a call', () => {
    const units = fold([call('read', 0), said([text('here it is')]), call('bash', 1)]);

    expect(kinds(units)).toEqual(['group', 'text', 'group']);
  });

  it('draws nothing for a monitor', () => {
    // The inspector owns monitors and the chat draws nothing for one, so a row
    // here would put a watcher in the conversation beside the calls it watches.
    const monitor = said([use('toolu_m', 'Monitor', { description: 'watch', command: 'tail -f' })]);
    const units = fold([call('read', 0), monitor, call('read', 1)]);

    expect(units).toHaveLength(1);
    const [group] = units;
    expect(group?.kind === 'group' ? group.families : []).toHaveLength(1);
    expect(group?.kind === 'group' ? group.families[0]?.calls.length : 0).toBe(2);
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
    expect(group?.kind === 'group' ? group.families.map((f) => f.label) : []).toEqual(['read']);
    expect(group?.kind === 'group' ? group.families[0]?.calls.length : 0).toBe(2);
  });

  it("reads an empty parent id as the session's own frame", () => {
    // The terminal reads the same field with the same guard: a frame carrying
    // an empty id is not a dispatch, and the wire does send them.
    const blank = said([text("the session's own line")], { parent_tool_use_id: '' });

    expect(fold([blank])).toHaveLength(1);
  });

  it('draws peer traffic as one message group, a lone message included', () => {
    // The strings are the producers' own, from `peers/types.rs`'s
    // `to_prose`: a test using a shape nothing writes is what let three
    // envelopes draw as the reader's own turn with the suite green.
    //
    // **And a lone message groups**, which is the one place this shape departs
    // from the terminal: its own `merge_messaging_groups` holds a group back
    // until it holds two, where this draws a group of one.
    const one = heard([
      text("[Message id=t-1 from agent 'steward' (org 'Busytools')]\n\nIT IMPORTED"),
    ]);
    const two = heard([
      text("[Message id=t-2 from agent 'planner' (org 'Busytools')]\n\npicking it up"),
    ]);

    expect(kinds(fold([one])), 'a lone message is a group of one').toEqual(['messages']);
    expect(kinds(fold([one, two]))).toEqual(['messages']);
    const [group] = fold([one, two]);
    expect(group?.kind === 'messages' ? group.lanes.length : 0, 'one kind, one lane').toBe(1);
    expect(group?.kind === 'messages' ? group.lanes[0]?.cards.length : 0).toBe(2);
  });

  it('gives each kind of peer traffic its own lane, in the order they arrived', () => {
    // Both trailers end with a clause inside the bracket - a question names the
    // tool to answer with, a reply says what it answers - so a matcher anchored
    // on the org clause's `)]` never fires and the envelope draws as the
    // reader's own turn: the orange panel with a raw header on screen.
    const ask = heard([
      text(
        "[Question id=q-1 from agent 'steward' (org 'Busytools') - reply with agents__tell in_reply_to=q-1]\n\nis the cron issue filed?",
      ),
    ]);
    const reply = heard([
      text(
        "[Reply id=t-2 from agent 'planner' (org 'Busytools') to your earlier ask]\n\ntaking the render half",
      ),
    ]);
    const message = heard([text("[Message id=t-3 from agent 'steward' (org 'Busytools')]\n\nFYI")]);

    const [group] = fold([ask, reply, message]);
    const lanes = group?.kind === 'messages' ? group.lanes : [];

    // A question draws on the ask lane whatever the wire calls it: the lane
    // word is the traffic's own, and both directions of a question share it.
    expect(lanes.map((lane) => lane.kind)).toEqual(['ask', 'reply', 'message']);
    expect(lanes.map((lane) => lane.cards.length)).toEqual([1, 1, 1]);
    expect(lanes[0]?.cards[0]?.body).toBe('is the cron issue filed?');
    expect(lanes[0]?.cards[0]?.peer).toBe('steward');
  });

  it('keeps one lane per kind however the kinds interleave', () => {
    // **Merged by kind, not by run**, which is what the terminal's own tally
    // draws - a lane per row and label over the whole group - and what keeps a
    // lane's word unique. Two lanes both headed `ask` collide on the key a view
    // opens the lane's leaves by, and Svelte refuses a duplicate key at mount:
    // the whole turn stops drawing, with nothing in an SSR render to show it.
    const ask = (id: string): unknown =>
      heard([
        text(
          `[Question id=${id} from agent 'steward' (org 'Busytools') - reply with agents__tell in_reply_to=${id}]\n\nis it filed?`,
        ),
      ]);
    const message = heard([text("[Message id=t-m from agent 'steward' (org 'Busytools')]\n\nFYI")]);

    const [group] = fold([ask('q-1'), message, ask('q-2')]);
    const lanes = group?.kind === 'messages' ? group.lanes : [];

    expect(
      lanes.map((lane) => lane.kind),
      'one lane per kind, first seen first',
    ).toEqual(['ask', 'message']);
    expect(
      lanes.map((lane) => lane.cards.length),
      'and both asks on the one lane',
    ).toEqual([2, 1]);
  });

  it('carries the id the message arrived with, which is what names its group', () => {
    // A handle whose uniqueness is not guaranteed is what has thrown twice in
    // this shape, and the wire's own id is the one field that separates two
    // messages from one sender.
    const arrived = heard([
      text("[Message id=t-9c1 from agent 'forge/steward' (org 'Busytools')]\n\nhi"),
    ]);
    const sent = said([
      {
        type: 'tool_use',
        id: 'toolu_01Bg',
        name: 'mcp__forge__agents__ask',
        input: { project: 'forge', label: 'steward', prompt: 'hi' },
      },
    ]);
    const cardOf = (units: Unit[]): { id?: string } | undefined =>
      units[0]?.kind === 'messages' ? units[0].lanes[0]?.cards[0] : undefined;

    expect(cardOf(fold([arrived]))?.id, 'an envelope is named by its own id').toBe('t-9c1');
    expect(cardOf(fold([sent]))?.id, 'and a call by the id the wire gave it').toBe('toolu_01Bg');
  });

  it('rolls a message group up from what each send came back with', () => {
    // The mark on a group is its aggregate, so a send that failed draws the
    // failure mark and one still out draws the ring - the sheet's own sentence,
    // "a failed delivery included", and what the terminal's `aggregate_status`
    // does for the same run.
    const ask = (id: string): unknown =>
      said([
        {
          type: 'tool_use',
          id,
          name: 'mcp__forge__agents__ask',
          input: { project: 'forge', label: 'steward', prompt: 'is it filed?' },
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
    const status = (units: Unit[]): string | null =>
      units[0]?.kind === 'messages' ? units[0].status : null;

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

    const [group] = fold([here, other, away], null, self);
    const cards = group?.kind === 'messages' ? (group.lanes[0]?.cards ?? []) : [];

    expect(
      cards.map((card) => card.here),
      'this project, then not',
    ).toEqual([true, false, false]);
    expect(
      cards.map((card) => card.org),
      'the org only where it is not the reader own',
    ).toEqual([null, null, 'Gateway']);
  });

  it('draws an external delivery as a notice rather than as a turn of the reader', () => {
    // A Gotify body sits ONE newline after the bracket: a title line, then the
    // message. A cron wrapper lands its prompt after `]\n\n`.
    const gotify = heard([text("[Gotify - app 'ci', priority 9]\nbuild failed\nrun 412")]);
    const cron = heard([text('[Cron]\n\nthe morning sweep')]);

    const units = fold([gotify, cron]);
    expect(kinds(units)).toEqual(['notice', 'notice']);
    const [first] = units;
    expect(first?.kind === 'notice' ? first.notice.severity : null).toBe('warning');
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('ci');
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('priority 9');
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('build failed');
    const second = units[1];
    expect(second?.kind === 'notice' ? second.notice.text : null, 'no leading blank line').toBe(
      'the morning sweep',
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

    const named = fold([line('steward')])[0];
    expect(named?.kind === 'notice' ? named.notice.text : '', 'a name is printed').toContain(
      'steward',
    );

    for (const id of ['U9', 'B09ABC123', 'C0C0T5E6RM1', 'DEPLOYS']) {
      const held = fold([line(id)])[0];
      expect(
        held?.kind === 'notice' ? held.notice.text : '',
        `${id} is an id, not a name`,
      ).not.toContain(id);
    }
  });

  it('draws a Slack bundle as a notice, bare channel and all', () => {
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
    expect(kinds(units)).toEqual(['notice', 'notice']);
    const [first] = units;
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('granite-staging-alerts');
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('steward');
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('the gate is green');
  });

  it('draws a failed delivery and a failed spawn as warnings', () => {
    const delivery = heard([
      text("[Ask id=q-1 to agent 'companies' (org 'Busytools') failed to deliver: channel closed]"),
    ]);
    const spawn = heard([text("[Worker 'planner' spawn failed id=w-1: ENOENT]")]);

    const units = fold([delivery, spawn]);
    expect(kinds(units)).toEqual(['notice', 'notice']);
    expect(units[0]?.kind === 'notice' ? units[0].notice.severity : null).toBe('warning');
    expect(units[0]?.kind === 'notice' ? units[0].notice.text : '').toContain('channel closed');
    expect(units[1]?.kind === 'notice' ? units[1].notice.severity : null).toBe('warning');
    expect(units[1]?.kind === 'notice' ? units[1].notice.text : '').toContain('ENOENT');
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

  it('keeps the question card even when nobody answered it', () => {
    const asked = said([
      use('toolu_q', 'AskUserQuestion', {
        questions: [{ question: 'Which colour?', options: [{ label: 'Red' }] }],
      }),
    ]);

    const units = fold([asked]);
    const [card] = units;
    expect(card?.kind === 'question' ? card.asked[0]?.question : null).toBe('Which colour?');
    expect(card?.kind === 'question' ? card.asked[0]?.picked_labels : null).toEqual([]);
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
    expect(kinds(units), 'the chip follows the run it came after').toEqual(['group', 'hooks']);
    const chip = units[1];
    expect(chip?.kind === 'hooks' ? chip.actions : null).toBe(1);
    expect(
      chip?.kind === 'hooks' ? chip.infos : [],
      'the command and the duration the captured row carries',
    ).toEqual([{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }]);
    expect(fold([hook(0)]), 'a frame reporting none draws nothing').toHaveLength(0);
  });

  it('settles each call with the result that answers it, and rolls the run up', () => {
    const units = fold([
      call('read', 0),
      heard([result('toolu_read_0', 'output')]),
      call('read', 1),
      heard([{ type: 'tool_result', tool_use_id: 'toolu_read_1', content: 'no', is_error: true }]),
    ]);

    const [group] = units;
    const calls = group?.kind === 'group' ? (group.families[0]?.calls ?? []) : [];
    expect(calls.map((leaf) => leaf.status)).toEqual(['completed', 'failed']);
    expect(group?.kind === 'group' ? group.status : null, 'and the run reports the failure').toBe(
      'failed',
    );
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
    expect(kinds(units), 'not a turn of the reader').toEqual(['messages']);
    const first = units[0];
    expect(first?.kind === 'messages' ? first.lanes[0]?.cards[0]?.peer : null).toBe('lead');
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
    const leaf = group?.kind === 'group' ? group.families[0]?.calls[0] : undefined;
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
    const call = group?.kind === 'group' ? group.families[0]?.calls[0] : undefined;
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
      expect(kinds(units), `no notice for ${why}`).toEqual(['group']);
      const held = units[0]?.kind === 'group' ? units[0].families[0]?.calls[0] : undefined;
      expect(held?.status, `and the turn still ended: ${why}`).toBe('failed');
    }

    // A dispatched agent's failure is not this turn's: its frames are the
    // SUBAGENTS surface's, and the drawing loop skips them, so a pre-pass that
    // read one would finalize a call the page never drew a row for.
    const child = { ...fatal, parent_tool_use_id: 'toolu_dispatch' };
    const units = fold([running, child]);
    const held = units[0]?.kind === 'group' ? units[0].families[0]?.calls[0] : undefined;
    expect(held?.status, 'a sub-agent failing says nothing about this turn').toBe('pending');
    expect(kinds(units), 'and draws no failure line here').toEqual(['group']);
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
    const calls = units.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );

    expect(calls.map((leaf) => leaf.id)).toEqual(['toolu_before', 'toolu_after']);
    expect(calls[0]?.status, 'the call the turn ended on is abandoned').toBe('failed');
    expect(calls[1]?.status, 'and the one it started afterwards is still out').toBe('pending');
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
    const calls = units.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );

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
    const calls = group?.kind === 'group' ? group.families.flatMap((one) => one.calls) : [];
    expect(
      calls.map((leaf) => leaf.status),
      'only the call left open failed',
    ).toEqual(['completed', 'failed']);
    expect(group?.kind === 'group' ? group.status : null, 'and the run says so').toBe('failed');
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
    const calls = group?.kind === 'group' ? group.families.flatMap((one) => one.calls) : [];
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
    const held = odd[0]?.kind === 'group' ? odd[0].families.flatMap((one) => one.calls) : [];
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
    const alone = orphan[0]?.kind === 'group' ? orphan[0].families.flatMap((one) => one.calls) : [];
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
    const calls = group?.kind === 'group' ? group.families.flatMap((one) => one.calls) : [];
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
    const odd = unknown[0]?.kind === 'group' ? unknown[0].families.flatMap((one) => one.calls) : [];
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
    const calls = units.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );

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
    const calls = units.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );

    // The absent field is its own case, and the corpus carries it: a
    // `local_workflow` task's `task_started` names no `is_backgrounded` at all,
    // and its notification carries a summary - read as anything but "not said
    // to be backgrounded", that workflow draws the duplicate line the case
    // above exists to remove. (`legacy-surface` carries the same shape with
    // `false`.)
    const unstated = fold([launch, { ...started, is_backgrounded: undefined }, notified]);
    const silent = unstated.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );
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
    const calls = units.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );
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
    const held = orphan.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );
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
    const calls = units.flatMap((unit) =>
      unit.kind === 'group' ? unit.families.flatMap((one) => one.calls) : [],
    );

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
    const call = group?.kind === 'group' ? group.families[0]?.calls[0] : undefined;
    expect(call?.status, 'the launch result is not the end of a backgrounded call').toBe(
      'in_progress',
    );
    expect(group?.kind === 'group' ? group.status : null, 'and the run says so').toBe(
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
    const done = first[0]?.kind === 'group' ? first[0].families[0]?.calls[0] : undefined;
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
    const dead = second[0]?.kind === 'group' ? second[0].families[0]?.calls[0] : undefined;
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
    const row = interrupted[0]?.kind === 'group' ? interrupted[0].families[0]?.calls[0] : undefined;
    expect(row?.status, 'an abandoned foreground call draws failed, not running').toBe('failed');

    // The flag-less shape the corpus carries (`workflow.jsonl`), whose task does
    // not say it outlives the turn either, and whose failing result still
    // settles the row.
    const unnamed = { ...started, is_backgrounded: undefined };
    const answered = heard([
      { type: 'tool_result', tool_use_id: 'toolu_fg', content: 'exit 1', is_error: true },
    ]);

    const settled = fold([launch, unnamed, answered]);
    const second = settled[0]?.kind === 'group' ? settled[0].families[0]?.calls[0] : undefined;
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

    expect(kinds(units)).toEqual(['user', 'group']);
    expect(units[0]?.kind === 'user' ? units[0].text : null).toBe('run the gate');
  });
});
