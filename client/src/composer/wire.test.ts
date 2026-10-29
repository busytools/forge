import { describe, expect, it } from 'vitest';

import { askFrom, composerFrom, resumeSessionsFrom } from './wire';

/** A question as the wire carries one, with whatever the test overrides in its prompt. */
function question(prompt: Record<string, unknown> = {}): unknown {
  return {
    kind: 'question',
    request: {
      tool_call: { tool_call_id: 'tu-q', raw_input: {} },
      prompt: {
        header: 'Environments',
        question: 'Pick the environments to deploy to.',
        multi_select: true,
        options: [
          {
            option_id: 'staging',
            label: 'Staging',
            description: 'The pre-production cluster',
            preview: 'deploy --env staging',
          },
        ],
        ...prompt,
      },
      question_index: 1,
      total_questions: 3,
    },
  };
}

/** A permission as the wire carries one, with the tool input the test names. */
function permission(rawInput: unknown): unknown {
  return {
    kind: 'permission',
    request: {
      tool_call: { tool_call_id: 'tu-p', title: 'Grep', raw_input: rawInput },
      display: { title: 'Grep', display_name: null, description: null, decision_reason: null },
      options: [],
    },
  };
}

/**
 * What the composer narrows for itself, at the boundary.
 *
 * The question's own fields are the ones it answers WITH: a multi-select
 * question toggles, so a dropped `multi_select` turns every question into a
 * single answer, and a dropped `description` leaves the row showing less than
 * the core sent.
 */
describe('the ask as the composer reads it', () => {
  it('carries what a question is answered with, and what its rows draw', () => {
    const ask = askFrom(question());
    if (ask?.kind !== 'question') throw new Error('the question did not narrow');

    expect(ask.request.multiSelect, 'a multi-select question toggles').toBe(true);
    expect(ask.request.options[0]?.description).toBe('The pre-production cluster');
    expect(ask.request.options[0]?.preview).toBe('deploy --env staging');
  });

  it('reads a single-select question as one that is not toggled', () => {
    const ask = askFrom(question({ multi_select: false }));
    if (ask?.kind !== 'question') throw new Error('the question did not narrow');
    expect(ask.request.multiSelect).toBe(false);
  });

  it('gives a permission a subject for the tools that name neither a command nor a path', () => {
    const grep = askFrom(permission({ pattern: 'fn main' }));
    if (grep?.kind !== 'permission') throw new Error('the permission did not narrow');
    expect(grep.request.subject, 'a pattern is what the call is about').toContain('fn main');

    const command = askFrom(permission({ command: 'ls -la' }));
    if (command?.kind !== 'permission') throw new Error('the permission did not narrow');
    expect(command.request.subject, 'and a named field still wins').toBe('ls -la');
  });

  it('names a held post rather than narrowing it to nothing', () => {
    const held = askFrom({
      kind: 'slack_draft',
      request: {
        id: '0192e1c0-0000-7000-8000-000000000000',
        workspace: 'Trust Machines',
        conversation: 'C0123',
        conversation_label: 'granite-staging-alerts',
        thread_ts: null,
        text: 'Deploy finished on staging.',
        tool: 'slack__post',
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(held.request.id, 'a draft is answered by its own id, not by a tool call').toBe(
      '0192e1c0-0000-7000-8000-000000000000',
    );
    expect(held.request.workspace).toBe('Trust Machines');
    expect(held.request.conversationLabel).toBe('granite-staging-alerts');
    expect(held.request.text).toBe('Deploy finished on staging.');
  });
});

describe('the browser hand-off read', () => {
  /**
   * **The tab is the ask.** It is what the person's Open raises, so a frame
   * whose tab this page cannot read opens the seat's own current tab rather
   * than a guessed number.
   */
  it('reads the tab a hand-off names, and nothing for one it cannot', () => {
    const named = askFrom({
      kind: 'browser_hand_off',
      request: { id: 'h1', reason: 'solve the CAPTCHA', tab: 3 },
    });
    expect(named).toEqual({
      kind: 'browser_hand_off',
      request: { id: 'h1', reason: 'solve the CAPTCHA', tab: 3 },
    });

    const unreadable = askFrom({
      kind: 'browser_hand_off',
      request: { id: 'h2', reason: 'solve the CAPTCHA', tab: 'three' },
    });
    expect(unreadable).toEqual({
      kind: 'browser_hand_off',
      request: { id: 'h2', reason: 'solve the CAPTCHA', tab: null },
    });
  });
});

/**
 * **The record's re-hand of a landing nobody sent parses as the landing it
 * is, marked carried** (#1360): the composer reads it off the record with
 * the same parser the live update's notice uses, and the flag is what makes
 * a reloaded page place words it never watched arrive.
 */
describe('the composer record', () => {
  it('reads a carried landing off the record', () => {
    const composer = composerFrom({
      take: null,
      notice: {
        kind: 'landed',
        text: 'the unsent words',
        truncated: false,
        generation: 7,
        carried: true,
      },
      machine: null,
      sign_in: null,
    });
    expect(composer.notice).toEqual({
      kind: 'landed',
      text: 'the unsent words',
      truncated: false,
      generation: 7,
      carried: true,
    });
  });

  it('reads no notice as none', () => {
    const composer = composerFrom({ take: null, notice: null, machine: null, sign_in: null });
    expect(composer.notice).toBeNull();
  });

  /**
   * **The machine turn narrows to one of the two this build draws, and a
   * state a newer server sends reads as none** - the least-alarming reading:
   * the composer stays usable rather than freezing on a word it cannot
   * explain.
   */
  it('narrows the machine turn and reads a state it does not know as none', () => {
    const at = (machine: unknown): unknown =>
      composerFrom({ take: null, notice: null, machine, sign_in: null }).machine;

    expect(at('handoff')).toBe('handoff');
    expect(at('compacting')).toBe('compacting');
    expect(at(null)).toBeNull();
    expect(at('summarising'), 'a state this build does not draw is not a freeze').toBeNull();

    // **A v15 server names the compaction only.** No `machine` key at all, so
    // the old boolean is the record's only word and a newer client beside an
    // older server is not blind.
    const old = composerFrom({ take: null, notice: null, compacting: true, sign_in: null });
    expect(old.machine, 'the v15 key is read as a compaction').toBe('compacting');
    expect(old.compacting, 'and it owns the box').toBe(true);
    const settled = composerFrom({ take: null, notice: null, compacting: false, sign_in: null });
    expect(settled.machine, 'and a v15 settle is no machine turn').toBeNull();

    // **A record that carries `machine` is authoritative**, including when it
    // says none: a v15 boolean left beside it must not resurrect a compaction
    // the record has already ended.
    const authoritative = composerFrom({
      take: null,
      notice: null,
      machine: null,
      compacting: true,
      sign_in: null,
    });
    expect(authoritative.machine, 'the present key wins over the leftover').toBeNull();
    expect(authoritative.compacting, 'and the leftover is not a compaction').toBe(false);
  });

  /**
   * **A surface this build does not know reads as absent**, the old-server
   * rule: the arm decides a live landing and the seat's carried words reach
   * the composer, rather than a newer client's name being refused by every
   * surface and losing the words.
   */
  it('reads a known surface, and drops a name this build does not know', () => {
    const at = (surface: string): unknown =>
      composerFrom({
        take: null,
        notice: {
          kind: 'landed',
          text: 'the unsent words',
          truncated: false,
          generation: 7,
          carried: true,
          surface,
        },
        machine: null,
        sign_in: null,
      }).notice;

    expect(at('review')).toEqual({
      kind: 'landed',
      text: 'the unsent words',
      truncated: false,
      generation: 7,
      carried: true,
      surface: 'review',
    });
    expect(at('from-a-newer-client'), 'an unknown name is absent, not a refusal').toEqual({
      kind: 'landed',
      text: 'the unsent words',
      truncated: false,
      generation: 7,
      carried: true,
    });
  });
});

/**
 * The `/resume` list's rows: a row whose id this client read under the wrong
 * key is a list that opens nothing, and nothing on screen says the field name
 * was the reason - while an id-less row is a pick that writes nothing.
 */
describe('the sessions a seat can resume', () => {
  it("reads each row's id and summary as the wire names them, in the order it sent", () => {
    expect(
      resumeSessionsFrom([
        { session_id: 'b-newer', summary: 'fix auth timeout' },
        { session_id: 'a-older', summary: 'resume picker' },
        { summary: 'no id at all' },
      ]),
      'the core orders the list, this keeps it, and a row with no id is not a pick',
    ).toEqual([
      { id: 'b-newer', summary: 'fix auth timeout' },
      { id: 'a-older', summary: 'resume picker' },
    ]);
  });

  it('reads nothing as an empty list, never a row', () => {
    expect(resumeSessionsFrom(undefined)).toEqual([]);
  });
});
