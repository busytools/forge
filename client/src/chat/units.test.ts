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
    // subagents section inside every turn that used one. The wire marks them
    // by the frame's sidechain flag, and the control rides in the same test -
    // the same frames without it ARE the conversation, so a fold that dropped
    // everything could not pass.
    const prose = said([text('the second one failed')]);
    const child = said([text('the second one failed')], { isSidechain: true });

    expect(fold([child])).toHaveLength(0);
    expect(fold([prose]), 'and the same frame is the conversation on its own').toHaveLength(1);
  });

  it('keeps a dispatched call out of the session group', () => {
    const dispatched = said([use('toolu_child', 'Grep', { pattern: 'x' })], { isSidechain: true });
    const units = fold([call('read', 0), dispatched, call('read', 1)]);

    expect(units).toHaveLength(1);
    const [group] = units;
    expect(group?.kind === 'group' ? group.families.map((f) => f.label) : []).toEqual(['read']);
    expect(group?.kind === 'group' ? group.families[0]?.calls.length : 0).toBe(2);
  });

  it('draws a peer run as one group and a lone message as the card it is', () => {
    const one = heard([text("[Message id=t-1 from agent 'steward' (org 'B')]\n\nIT IMPORTED")]);
    const two = heard([text("[Message id=t-2 from agent 'planner' (org 'B')]\n\npicking it up")]);

    expect(kinds(fold([one]))).toEqual(['peer']);
    expect(kinds(fold([one, two]))).toEqual(['peers']);
    const [peers] = fold([one, two]);
    expect(peers?.kind === 'peers' ? peers.cards.length : 0).toBe(2);
  });

  it('draws an external delivery as a notice rather than as a turn of the reader', () => {
    const gotify = heard([text("[Gotify - app 'ci', priority 9]\n\nbuild failed")]);
    const cron = heard([text('[Cron] the morning sweep')]);

    const units = fold([gotify, cron]);
    expect(kinds(units)).toEqual(['notice', 'notice']);
    const [first] = units;
    expect(first?.kind === 'notice' ? first.notice.severity : null).toBe('warning');
    expect(first?.kind === 'notice' ? first.notice.source : null).toBe('gotify');
    expect(first?.kind === 'notice' ? first.notice.text : '').toContain('build failed');
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
    const hook = (actions: number): unknown => ({
      type: 'system',
      subtype: 'stop_hook_summary',
      actions,
      hook_infos: [],
      uuid: 'hooks-1',
    });

    expect(kinds(fold([call('read', 0), hook(2)]))).toEqual(['group', 'hooks']);
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
