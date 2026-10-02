/**
 * The composer's test fixtures: the props a session page would build, and a
 * connection that records what it was sent.
 *
 * A plain module rather than a rune-bearing one, because the reactive half of
 * the harness is `Harness.svelte`: runes compile in `.svelte` and `.svelte.ts`
 * files, and a `.svelte.ts` module is TypeScript with runes in it, which the
 * TypeScript parser reads as a syntax error.
 *
 * Nothing the app ships imports this file.
 */

import type { ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import type { ComposerProps, ComposerRecord, SeatRead } from './view';

export const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** A record with nothing in it, which is the seat the tests start from. */
export function record(over: Partial<ComposerRecord> = {}): ComposerRecord {
  return {
    composer: { take: null, notice: null, compacting: false, sign_in: null },
    dictate_overrides: { styling: null, structure: null, context: null },
    pending_ask: null,
    header: { turn_in_flight: false },
    slash_commands: [],
    subagents: [],
    file_index: { entries: {} },
    ...over,
  };
}

/** A seat that is running and taking input, which is the one that is not blocked. */
export function seatRead(over: Partial<SeatRead> = {}): SeatRead {
  return { lifecycle: 'Running', reason: null, waking: false, pendingDepth: 1, ...over };
}

/** The props a page hands the composer, which every test starts from. */
export function props(over: Partial<ComposerProps> = {}): ComposerProps {
  return {
    record: record(),
    slot: SLOT,
    seat: seatRead(),
    connection: wire().connection,
    dictation: false,
    ...over,
  };
}

/**
 * A take in flight, as the WIRE carries one.
 *
 * Snake-case and a two-slot `progress`, because this goes into a record the
 * composer narrows: a fixture in the narrowed shape reads as a take with no
 * clock, no level and no readings, which is what the composer's own by-width
 * page showed before this was fixed.
 */
export function take(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    phase: 'recording',
    levels: [0.2, 0.5, 1],
    peak_db: -18,
    progress: [0, null],
    floor_db: -50,
    elapsed_ms: 7000,
    ...over,
  };
}

/** A permission request as the core offers one, which is what the dock draws. */
export function permissionAsk(toolId = 'tu-1'): unknown {
  return {
    kind: 'permission',
    request: {
      tool_call: {
        tool_call_id: toolId,
        title: 'Bash',
        kind: 'execute',
        status: 'pending',
        content: [],
        locations: [],
        raw_input: { command: 'git push origin main' },
      },
      display: {
        title: 'Bash',
        display_name: null,
        description: 'Runs a command in your shell.',
        decision_reason: null,
      },
      options: [
        { option_id: 'opt-once', name: 'Allow once', kind: 'allow', action: { kind: 'allow' } },
        { option_id: 'opt-deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
        {
          option_id: 'opt-notes',
          name: 'Tell Claude something else',
          kind: 'notes',
          action: { kind: 'deny' },
        },
      ],
    },
  };
}

/**
 * A question as the core offers one, with whatever the test overrides in its
 * prompt.
 *
 * The index is a parameter because one tool call carries every question in a
 * batch: the core reuses the tool id and advances this, so a test that needs two
 * questions of one call needs two of these.
 */
export function questionAsk(
  toolId = 'tu-q',
  prompt: Record<string, unknown> = {},
  index = 0,
  total = 1,
): unknown {
  return {
    kind: 'question',
    request: {
      tool_call: {
        tool_call_id: toolId,
        title: 'AskUserQuestion',
        kind: 'other',
        status: 'pending',
        content: [],
        locations: [],
        raw_input: {},
      },
      prompt: {
        header: 'Environments',
        question: 'Pick the environments to deploy to.',
        multi_select: true,
        options: [
          {
            option_id: 'q-staging',
            label: 'Staging',
            description: 'The pre-production cluster',
            preview: 'deploy --env staging',
          },
          { option_id: 'q-prod', label: 'Production', description: null, preview: null },
        ],
        ...prompt,
      },
      question_index: index,
      total_questions: total,
    },
  };
}

/** A held Slack post as the core offers one, with whatever the test overrides. */
export function slackDraftAsk(over: Record<string, unknown> = {}): unknown {
  return {
    kind: 'slack_draft',
    request: {
      id: '0192e1c0-0000-7000-8000-000000000000',
      workspace: 'Trust Machines',
      conversation: 'C0123456789',
      conversation_label: 'granite-staging-alerts',
      thread_ts: null,
      text: 'Deploy finished on staging.',
      tool: 'slack__post',
      ...over,
    },
  };
}

/** One command the composer sent, as the connection received it. */
export interface Sent {
  command: Record<string, Record<string, unknown>>;
}

/** One connection two composers can be mounted on, which is two clients on one seat. */
export interface Wire {
  sent: Sent[];
  /** How many times a panel asked for the device list, which one walk each. */
  asked: number;
  connection: Pick<Connection, 'dispatch' | 'onMessage' | 'devices'>;
  /** Say something to every composer attached, as the server would. */
  say(message: ServerMessage): void;
}

/**
 * A connection that records what it was sent and can be made to speak.
 *
 * Only the two things the composer uses of one, which is also why the composer
 * declares its own narrow type: a fake that had to satisfy the whole
 * `Connection` would be scaffolding nothing in these tests exercises.
 */
export function wire(): Wire {
  const sent: Sent[] = [];
  const listeners = new Set<(message: ServerMessage) => void>();
  const held: Wire = {
    sent,
    asked: 0,
    connection: {
      dispatch(command: Record<string, Record<string, unknown>>) {
        sent.push({ command });
        return null;
      },
      devices() {
        held.asked += 1;
        return true;
      },
      onMessage(fn: (message: ServerMessage) => void) {
        listeners.add(fn);
        return () => listeners.delete(fn);
      },
    },
    say(message) {
      for (const fn of listeners) fn(message);
    },
  };
  return held;
}
