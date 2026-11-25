// @vitest-environment jsdom
/**
 * A steering prompt keeps one row, at its delivered place, when a page lands
 * (#3073).
 *
 * The reader sends a message while a turn is running. The live path draws it
 * when the CLI starts it, in the seat's sequence - but that sequence is when
 * the lifecycle event fired, not where the CLI recorded the delivered row. A
 * page read later carries the same words under the id the CLI minted for its
 * own user row, at the position the transcript recorded. The page is the
 * authority once it lands: the row moves at most once, to its delivered
 * place, and no later landing moves it or draws a second copy.
 */
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat } from './conversation';

const SEAT: SessionSlot = { org: 'Scratch', project: 'steer', label: 'lead' };
const STEER = 'STEERING-VERIFY-ROW: while that is running, also reply with the single word beta.';
const CLI_STEER = '77e9443d-5902-422c-85d7-6318689d1b9f';

afterEach(() => {
  vi.useRealTimers();
});

/** A connection a test drives by hand; the chat reads only these three things. */
function fakeConnection() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'loading' }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (_conversation: SessionSlot, _before: string | null, _turns: number) => true,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      statuses.add(fn);
      return () => statuses.delete(fn);
    },
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;
  return {
    connection,
    send(message: ServerMessage): void {
      for (const fn of listeners) fn(message);
    },
    update(update: SessionUpdate): void {
      this.send({ kind: 'update', update });
    },
  };
}

const userText = (id: string, text: string): unknown => ({
  type: 'user',
  uuid: id,
  message: { role: 'user', content: [{ type: 'text', text }] },
});

/** The CLI's own row for a delivered prompt: the id it minted, not the wire's. */
const clipped = (id: string, text: string): unknown => ({
  type: 'user',
  uuid: id,
  message: { role: 'user', content: text },
});

const thinking = (id: string): unknown => ({
  type: 'assistant',
  uuid: id,
  message: {
    id: `m-${id}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'thinking', thinking: 'hmm' }],
  },
});

const said = (id: string, text: string): unknown => ({
  type: 'assistant',
  uuid: id,
  message: {
    id: `m-${id}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text }],
  },
});

const called = (id: string, call: string, name: string): unknown => ({
  type: 'assistant',
  uuid: id,
  message: {
    id: `m-${id}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'tool_use', id: call, name, input: {} }],
  },
});

const answered = (id: string, call: string): unknown => ({
  type: 'user',
  uuid: id,
  message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: call, content: 'ok' }] },
});

const ended = (id: string): unknown => ({ type: 'result', subtype: 'success', uuid: id });

/**
 * The live seat's frames, as the sequence fires them.
 *
 * The steering row is drawn BEFORE the turn's second call: the lifecycle event
 * the live path follows fires when the CLI starts the prompt, which here is
 * ahead of the frame for the call already in flight. The read records the
 * delivered row after that call - the divergence #3073 is about.
 */
function liveFrames(): SessionUpdate[] {
  const frame = (msg: unknown, source?: string): SessionUpdate => ({
    chat_appended: { key: SEAT, msg, ...(source === undefined ? {} : { source }) },
  });
  return [
    frame(userText('forge_prompt_head', 'Run `sleep 100` in the foreground, then reply alpha.')),
    frame(thinking('th-1')),
    frame(called('call-1-msg', 'call-1', 'Bash')),
    frame(answered('result-1', 'call-1')),
    frame(userText('forge_prompt_steer', STEER), 'you'),
    frame(called('call-2-msg', 'call-2', 'Read')),
    frame(thinking('th-2')),
    frame(said('said-2', 'alpha beta')),
    frame(ended('result-2')),
  ];
}

/** The page the read serves: the steering row under the CLI's own id, after call-2. */
const page = (): ServerMessage =>
  ({
    kind: 'snapshot',
    subject: { session: SEAT } as Subject,
    data: {
      header: { session_id: 'scratch', turn_in_flight: false },
      conversation: {
        cursor: null,
        turns: [
          {
            key: 't1',
            messages: [
              userText('forge_prompt_head', 'Run `sleep 100` in the foreground, then reply alpha.'),
              thinking('th-1'),
              called('call-1-msg', 'call-1', 'Bash'),
              answered('result-1', 'call-1'),
              called('call-2-msg', 'call-2', 'Read'),
            ],
          },
          {
            key: 't2',
            messages: [
              clipped(CLI_STEER, STEER),
              thinking('th-2'),
              said('said-2', 'alpha beta'),
              ended('result-2'),
            ],
          },
        ],
      },
    },
  }) as unknown as ServerMessage;

/** Every message's id, flattened across the turns in order, user rows marked. */
function drawnIds(chat: Chat): string[] {
  return get(chat.value)
    .turns.flatMap((turn) => turn.messages)
    .map((message) => (message as { uuid?: string }).uuid ?? '?');
}

function steeringRows(chat: Chat): number {
  return get(chat.value)
    .turns.flatMap((turn) => turn.messages)
    .filter((message) => {
      const held = message as { type?: string; uuid?: string; message?: { content?: unknown } };
      if (held.type !== 'user') return false;
      const content = held.message?.content;
      return Array.isArray(content)
        ? content.some((block) => (block as { text?: unknown }).text === STEER)
        : content === STEER;
    }).length;
}

describe('a steering row across a page landing', () => {
  it('moves once to its delivered place and stays there on later landings', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    for (const update of liveFrames()) server.update(update);

    const before = drawnIds(chat);
    const liveSteer = before.indexOf('forge_prompt_steer');
    const call2 = before.indexOf('call-2-msg');
    expect(liveSteer, 'the live fold drew the steering row').toBeGreaterThanOrEqual(0);
    expect(liveSteer, 'live put it before the call the sequence fired it ahead of').toBeLessThan(
      call2,
    );

    server.send(page());

    const after = drawnIds(chat);
    expect(steeringRows(chat), 'one prompt, one row').toBe(1);
    expect(after.includes('forge_prompt_steer'), 'the live copy is the one that goes').toBe(false);
    const delivered = after.indexOf(CLI_STEER);
    expect(delivered, 'the page row drew').toBeGreaterThanOrEqual(0);
    expect(after[delivered - 1], 'it sits after the call the transcript recorded before it').toBe(
      'call-2-msg',
    );

    // A refresh, a reconnect or a scroll-up re-fetches the same page. The row
    // is already where the read put it, so nothing moves and nothing doubles.
    server.send(page());
    expect(drawnIds(chat), 'no move on a later landing').toEqual(after);
    expect(steeringRows(chat), 'and no second copy').toBe(1);
    stop();
  });

  it('does not pair a held row with a page row from another turn', () => {
    // The held display row is turn B's. A page that lands an OLDER turn whose
    // user text repeats the same words is another message, and a pairing that
    // reaches across turns consumes the wrong held row - a row lost on an
    // ordinary scroll-up.
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    const frame = (msg: unknown, source?: string): SessionUpdate => ({
      chat_appended: { key: SEAT, msg, ...(source === undefined ? {} : { source }) },
    });
    server.update(frame(userText('a1', 'an older question')));
    server.update(frame(said('a2', 'an older answer')));
    server.update(frame(ended('a-end')));
    server.update(frame(userText('head-2', 'Run the slow command')));
    server.update(frame(userText('forge_prompt_steer', STEER), 'you'));
    server.update(frame(thinking('th-3')));
    server.update(frame(said('s-3', 'done')));
    server.update(frame(ended('r-3')));

    // The page carries the older turn with the same words under a CLI id, and
    // a turn B read BEFORE the delivery: no steering row of its own.
    server.send({
      kind: 'snapshot',
      subject: { session: SEAT } as Subject,
      data: {
        header: { session_id: 'scratch', turn_in_flight: false },
        conversation: {
          cursor: null,
          turns: [
            {
              key: 'ta',
              messages: [clipped('cli-old', STEER), said('a2', 'an older answer'), ended('a-end')],
            },
            {
              key: 'tb',
              messages: [
                userText('head-2', 'Run the slow command'),
                thinking('th-3'),
                said('s-3', 'done'),
                ended('r-3'),
              ],
            },
          ],
        },
      },
    } as unknown as ServerMessage);

    const ids = drawnIds(chat);
    expect(ids.includes('forge_prompt_steer'), 'the held row survives the landing').toBe(true);
    expect(steeringRows(chat), 'one live row, and the older question is its own').toBe(2);
    stop();
  });

  it('lands a page-only steering row where its page row puts it', () => {
    // The live fold missed the delivery (a page that attached after it), so the
    // page's row is the only copy - and it belongs between the call it followed
    // and the frames that came after, not at the turn's head.
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    for (const update of liveFrames()) {
      const msg = (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg as
        { uuid?: string } | undefined;
      if (msg?.uuid === 'forge_prompt_steer') continue;
      server.update(update);
    }

    server.send(page());

    const ids = drawnIds(chat);
    const at = ids.indexOf(CLI_STEER);
    expect(at, 'the page row drew').toBeGreaterThanOrEqual(0);
    expect(ids[at - 1], 'it follows the call its page row followed').toBe('call-2-msg');
    expect(ids[at + 1], 'it leads the thinking that came after').toBe('th-2');
    stop();
  });
});
