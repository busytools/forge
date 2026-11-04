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
        operation: 'delete',
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(held.request.id, 'a draft is answered by its own id, not by a tool call').toBe(
      '0192e1c0-0000-7000-8000-000000000000',
    );
    expect(held.request.workspace).toBe('Trust Machines');
    expect(held.request.conversationLabel).toBe('granite-staging-alerts');
    expect(held.request.text).toBe('Deploy finished on staging.');
    expect(
      held.request.operation,
      'the operation is what the card words its verbs from, so dropping it here silently draws post copy',
    ).toBe('delete');
  });

  it('reads the thread and the tagged people a held draft was resolved with', () => {
    const held = askFrom({
      kind: 'slack_draft',
      request: {
        id: '0192e1c0-0000-7000-8000-000000000000',
        workspace: 'Trust Machines',
        conversation: 'C0123',
        conversation_label: 'granite-staging-alerts',
        thread_ts: '1758901234.482910',
        text: 'Deploy finished <@U2>.',
        operation: 'post',
        thread: { author: 'alice', excerpt: 'the staging deploy is green...' },
        tagged: [
          { id: 'U2', name: 'bob' },
          { id: 'U3', name: null },
        ],
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(held.request.thread, 'the thread is named core-side, not by this page').toEqual({
      author: 'alice',
      excerpt: 'the staging deploy is green...',
    });
    expect(held.request.tagged, 'an unresolved tag keeps its id rather than being dropped').toEqual(
      [
        { id: 'U2', name: 'bob' },
        { id: 'U3', name: null },
      ],
    );
  });

  it('reads a draft from a server that predates the thread and tag fields', () => {
    const held = askFrom({
      kind: 'slack_draft',
      request: {
        id: '0192e1c0-0000-7000-8000-000000000000',
        workspace: 'Trust Machines',
        conversation: 'C0123',
        conversation_label: 'granite-staging-alerts',
        thread_ts: '1758901234.482910',
        text: 'Deploy finished on staging.',
        operation: 'post',
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(
      held.request.thread,
      'no thread name reads as none, so the card draws the ts',
    ).toBeNull();
    expect(held.request.tagged, 'and no tags reads as none, not as a placeholder').toEqual([]);
  });

  it('reads a malformed thread or tag row as nothing it can draw', () => {
    const held = askFrom({
      kind: 'slack_draft',
      request: {
        id: '0192e1c0-0000-7000-8000-000000000000',
        workspace: 'Trust Machines',
        conversation: 'C0123',
        conversation_label: '#granite-staging-alerts',
        thread_ts: '1758901234.482910',
        text: 'Deploy finished <@U2>.',
        operation: 'post',
        // A thread with no author cannot be drawn without inventing one.
        thread: { excerpt: 'no author here' },
        // A tag with no id is not a person; a row that names one survives.
        tagged: [{ name: 'no id' }, { id: 'U2', name: 'bob' }, 'junk'],
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(held.request.thread, 'an authorless thread is no thread name').toBeNull();
    expect(held.request.tagged, 'only rows that name a person are tags').toEqual([
      { id: 'U2', name: 'bob' },
    ]);
  });

  it('reads whitespace-only names and ids as nothing to draw', () => {
    const held = askFrom({
      kind: 'slack_draft',
      request: {
        id: '0192e1c0-0000-7000-8000-000000000000',
        workspace: 'Trust Machines',
        conversation: 'C0123',
        conversation_label: '#granite-staging-alerts',
        thread_ts: '1758901234.482910',
        text: 'Deploy finished <@U2>.',
        operation: 'post',
        thread: { author: '   ', excerpt: 'a line with no author' },
        tagged: [
          { id: '  ', name: 'nobody' },
          { id: 'U2', name: '   ' },
        ],
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(held.request.thread, 'a blank author is not a name').toBeNull();
    expect(held.request.tagged, 'a blank id is not a person and a blank name is none').toEqual([
      { id: 'U2', name: null },
    ]);
  });

  it('narrows an operation this client is older than to neutral, not a post', () => {
    const held = askFrom({
      kind: 'slack_draft',
      request: {
        id: '0192e1c0-0000-7000-8000-000000000000',
        workspace: 'Trust Machines',
        conversation: 'C0123',
        conversation_label: 'granite-staging-alerts',
        thread_ts: null,
        text: 'Deploy finished on staging.',
        operation: 'archive',
      },
    });
    if (held?.kind !== 'slack_draft') throw new Error('the held post did not narrow');

    expect(held.request.operation, 'an unknown value must not borrow a post verb').toBe('unknown');
  });

  it('names a held config write with the key and both values', () => {
    const held = askFrom({
      kind: 'settings_write',
      request: {
        id: '0192e1c0-0000-7000-8000-0000000000bb',
        key: 'orgs.Default.projects.forge.max_workers',
        current: 2,
        proposed: 4,
        unset: false,
      },
    });
    if (held?.kind !== 'settings_write') throw new Error('the held write did not narrow');

    expect(held.request.id).toBe('0192e1c0-0000-7000-8000-0000000000bb');
    expect(held.request.key).toBe('orgs.Default.projects.forge.max_workers');
    expect(held.request.current).toBe(2);
    expect(held.request.proposed).toBe(4);
    expect(held.request.unset).toBe(false);
  });

  it('names a held vault call by the facts its card draws, and nothing else', () => {
    // **A value planted on an unknown key never reaches the narrowed shape**,
    // which is the client's half of the draft's contract: the core sends no
    // secret, and this parser could not carry one if it did.
    const held = askFrom({
      kind: 'vault',
      request: {
        id: '0192e1c0-0000-7000-8000-0000000000cc',
        tool: 'vault__update',
        summary: "change the login for 'bank'",
        item_id: 'cipher-id',
        item_name: 'bank',
        field: 'password',
        masked_diff: 'password: ********',
        attachment: { name: 'notes.txt', size: 97, replaces: 'old-attachment' },
        needs_unlock: true,
        secret: 'hunter2',
      },
    });
    if (held?.kind !== 'vault') throw new Error('the held call did not narrow');

    expect(held.request).toEqual({
      id: '0192e1c0-0000-7000-8000-0000000000cc',
      tool: 'vault__update',
      summary: "change the login for 'bank'",
      itemId: 'cipher-id',
      itemName: 'bank',
      field: 'password',
      maskedDiff: 'password: ********',
      attachment: { name: 'notes.txt', size: 97, replaces: 'old-attachment' },
      needsUnlock: true,
    });
  });
});

describe('the browser hand-off read', () => {
  /**
   * **The page is the ask.** Open resolves the hand-off's page id, which is
   * the tab's identity across a host replacement; the tab is what the bar
   * names. A frame whose values this page cannot read opens the seat's own
   * current tab rather than a guessed number.
   */
  it('reads the page and tab a hand-off names, and nothing for ones it cannot', () => {
    const named = askFrom({
      kind: 'browser_hand_off',
      request: { id: 'h1', reason: 'solve the CAPTCHA', tab: 3, target: 'page-3' },
    });
    expect(named).toEqual({
      kind: 'browser_hand_off',
      request: { id: 'h1', reason: 'solve the CAPTCHA', tab: 3, target: 'page-3' },
    });

    const unreadable = askFrom({
      kind: 'browser_hand_off',
      request: { id: 'h2', reason: 'solve the CAPTCHA', tab: 'three', target: 7 },
    });
    expect(unreadable).toEqual({
      kind: 'browser_hand_off',
      request: { id: 'h2', reason: 'solve the CAPTCHA', tab: null, target: null },
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
    const composer = composerFrom({ take: null, notice: null, machine: null });
    expect(composer.notice).toBeNull();
  });

  /**
   * **The record's take keeps its generation**, which is what tells its own
   * landing from an older re-hand when a read arrives across the landing
   * (#2283). Dropping the parse leaves every test above green and reverts the
   * landing memory at all four surfaces.
   */
  it("keeps the take's own generation off the record", () => {
    const composer = composerFrom({
      take: { phase: 'recording', levels: [], peak_db: -50, progress: [0, null], generation: 4 },
      notice: null,
      machine: null,
    });
    expect(composer.take?.generation, 'the take names its own number').toBe(4);
  });

  /**
   * **The machine turn narrows to one of the two this build draws, and a
   * state a newer server sends reads as none** - the least-alarming reading:
   * the composer stays usable rather than freezing on a word it cannot
   * explain.
   */
  it('narrows the machine turn and reads a state it does not know as none', () => {
    const at = (machine: unknown): unknown =>
      composerFrom({ take: null, notice: null, machine }).machine;

    expect(at('handoff')).toBe('handoff');
    expect(at('compacting')).toBe('compacting');
    expect(at(null)).toBeNull();
    expect(at('summarising'), 'a state this build does not draw is not a freeze').toBeNull();

    // **A v15 server names the compaction only.** No `machine` key at all, so
    // the old boolean is the record's only word and a newer client beside an
    // older server is not blind.
    const old = composerFrom({ take: null, notice: null, compacting: true });
    expect(old.machine, 'the v15 key is read as a compaction').toBe('compacting');
    expect(old.compacting, 'and it owns the box').toBe(true);
    const settled = composerFrom({ take: null, notice: null, compacting: false });
    expect(settled.machine, 'and a v15 settle is no machine turn').toBeNull();

    // **A record that carries `machine` is authoritative**, including when it
    // says none: a v15 boolean left beside it must not resurrect a compaction
    // the record has already ended.
    const authoritative = composerFrom({
      take: null,
      notice: null,
      machine: null,
      compacting: true,
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
