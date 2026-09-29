/**
 * The composer's test harness: the props a session page would hand it, held so
 * a test can change them the way a page re-renders.
 *
 * **`$state`, and the reason is the behaviour under test.** The composer reads
 * its props reactively and holds the reader's draft in its own state, so a test
 * that mounts it again for a second record would be testing a fresh component
 * with a fresh draft - which is precisely the defect the draft tests exist to
 * catch. Mutating this object is what a page re-render is.
 *
 * Nothing the app ships imports this file.
 */

import type { Connection } from '../socket';
import type { ServerMessage } from '../protocol';
import type { SessionSlot } from '../wire/types';
import type { ComposerProps, ComposerRecord, SeatRead } from './view';
import type { Take } from './wire';

export const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** A record with nothing in it, which is the seat the tests start from. */
export function record(over: Partial<ComposerRecord> = {}): ComposerRecord {
  return {
    composer: { take: null, notice: null, compacting: false, sign_in: null },
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

/** A take in flight, as the core reports one. */
export function take(over: Partial<Take> = {}): Take {
  return {
    phase: 'recording',
    levels: [0.2, 0.5, 1],
    peakDb: -18,
    progress: { done: 0, total: null },
    elapsedMs: 7000,
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
        {
          option_id: 'opt-once',
          name: 'Allow once',
          kind: 'allow',
          action: { kind: 'allow' },
        },
        { option_id: 'opt-deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
        { option_id: 'opt-notes', name: 'Tell Claude something else', kind: 'notes', action: { kind: 'deny' } },
      ],
    },
  };
}

/** A question as the core offers one. */
export function questionAsk(toolId = 'tu-q'): unknown {
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
          { option_id: 'q-staging', label: 'Staging', description: null, preview: null },
          { option_id: 'q-prod', label: 'Production', description: null, preview: null },
        ],
      },
      question_index: 1,
      total_questions: 3,
    },
  };
}

/** One command the composer sent, as the connection received it. */
export interface Sent {
  command: Record<string, Record<string, unknown>>;
}

/** Everything a fake connection recorded, and the two ways a test drives it. */
export interface Fake {
  props: ComposerProps;
  sent: Sent[];
  /** Say something to the composer, as the server would. */
  say(message: ServerMessage): void;
}

/** One connection two composers can be mounted on, which is two clients on one seat. */
export interface Wire {
  sent: Sent[];
  connection: Pick<Connection, 'dispatch' | 'onMessage'>;
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
  return {
    sent,
    connection: {
      dispatch(command: Record<string, Record<string, unknown>>) {
        sent.push({ command });
        return null;
      },
      onMessage(fn: (message: ServerMessage) => void) {
        listeners.add(fn);
        return () => listeners.delete(fn);
      },
    } as unknown as Pick<Connection, 'dispatch' | 'onMessage'>,
    say(message) {
      for (const fn of listeners) fn(message);
    },
  };
}

/**
 * The props one composer is mounted with, over `on` when a test is driving two
 * of them against one seat.
 */
export function fake(over: Partial<ComposerProps> = {}, on: Wire = wire()): Fake {
  const props = $state<ComposerProps>({
    record: record(),
    slot: SLOT,
    seat: seatRead(),
    connection: on.connection,
    ...over,
  });

  return { props, sent: on.sent, say: on.say };
}
