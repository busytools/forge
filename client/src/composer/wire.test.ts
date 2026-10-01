import { describe, expect, it } from 'vitest';

import { askFrom } from './wire';

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
    expect(held.request.tool, 'which tool is what is actually waiting').toBe('slack__post');
    expect(held.request.text).toBe('Deploy finished on staging.');
  });
});
