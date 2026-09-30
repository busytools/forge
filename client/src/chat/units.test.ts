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
      expect(kinds(units)[1], 'and that call is the unit in the middle').toMatch(/question|peer/);
    }
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

  it('draws a peer run as one group and a lone message as the card it is', () => {
    // The strings are the producers' own, from `peers/types.rs`'s
    // `to_prose`: a test using a shape nothing writes is what let three
    // envelopes draw as the reader's own turn with the suite green.
    const one = heard([
      text("[Message id=t-1 from agent 'steward' (org 'Busytools')]\n\nIT IMPORTED"),
    ]);
    const two = heard([
      text("[Message id=t-2 from agent 'planner' (org 'Busytools')]\n\npicking it up"),
    ]);

    expect(kinds(fold([one]))).toEqual(['peer']);
    expect(kinds(fold([one, two]))).toEqual(['peers']);
    const [peers] = fold([one, two]);
    expect(peers?.kind === 'peers' ? peers.cards.length : 0).toBe(2);
  });

  it("draws a question and a reply that carry the producer's trailer", () => {
    // Both end with a clause inside the bracket - a question names the tool to
    // answer with, a reply says what it answers - so a matcher anchored on the
    // org clause's `)]` never fires and the envelope draws as the reader's own
    // turn: the orange panel with a raw header on screen.
    const question = heard([
      text(
        "[Question id=q-1 from agent 'steward' (org 'Busytools') - reply with agents__tell in_reply_to=q-1]\n\nis the cron issue filed?",
      ),
    ]);
    const reply = heard([
      text(
        "[Reply id=t-2 from agent 'planner' (org 'Busytools') to your earlier ask]\n\ntaking the render half",
      ),
    ]);

    const units = fold([question, reply]);
    expect(kinds(units), 'neither is a turn of the reader').toEqual(['peers']);
    const [peers] = units;
    const cards = peers?.kind === 'peers' ? peers.cards : [];
    expect(cards[0]?.kind).toBe('question');
    expect(cards[0]?.peer).toBe('steward');
    expect(cards[0]?.body).toBe('is the cron issue filed?');
    expect(cards[1]?.kind).toBe('reply');
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
    expect(kinds(units), 'not a turn of the reader').toEqual(['peer']);
    expect(units[0]?.kind === 'peer' ? units[0].card.peer : null).toBe('lead');
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
    expect(kinds(pdf)).toEqual(['user']);
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

    // Two frames that carry no words to draw, and each for its own reason: a
    // frame whose string is blank draws an empty notice, and one whose `error`
    // is not a string at all draws a row reading `null`. Both are still the
    // end of the turn, so the call draws failed either way.
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

  it('says a failed turn failed, and finalizes the call it left open', () => {
    // The result frame is the interrupt capture's own (interrupt.jsonl): its
    // fields verbatim, and its prompt carries no tool call - the call here is
    // what an interrupt catches, which is the shape the capture shows rather
    // than one it carries.
    //
    // **And a answered call rides beside the open one**, because that is the
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
