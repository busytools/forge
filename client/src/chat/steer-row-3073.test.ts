// @vitest-environment jsdom
/**
 * A steering prompt keeps its place when a page lands (#3073).
 *
 * The reader sends a message while a turn is running. The live path draws it
 * when the CLI starts it, in the seat's sequence, between the tool result it
 * followed and the frames that came after. A page read later carries the same
 * words under the id the CLI minted for its own user row - not the id the
 * live row was sent under - so a landing that cannot pair the two is what
 * moves the row or draws it twice.
 *
 * **The invariant is the flattened order**, not the turn cut: a landing may
 * re-cut which turn a row belongs to, but a row already drawn stays where it
 * was, and one prompt is one row.
 */
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat } from './conversation';

const SEAT: SessionSlot = { org: 'Scratch', project: 'steer', label: 'lead' };
const STEER = 'STEERING-VERIFY-ROW: while that is running, also reply with the single word beta.';

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

/** The live seat's frames for one prompt, a tool round, and a steering send. */
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
    frame(thinking('th-2')),
    frame(said('said-2', 'alpha beta')),
    frame(ended('result-2')),
  ];
}

/** The page the read serves: the steering row under the CLI's own id, at its recorded index. */
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
            ],
          },
          {
            key: 't2',
            messages: [
              clipped('77e9443d-5902-422c-85d7-6318689d1b9f', STEER),
              thinking('th-2'),
              said('said-2', 'alpha beta'),
              ended('result-2'),
            ],
          },
        ],
      },
    },
  }) as unknown as ServerMessage;

/** Every drawn row's words and id, flattened across the turns in order. */
function flattened(chat: Chat): { id: string | null; words: string }[] {
  const held = get(chat.value);
  const out: { id: string | null; words: string }[] = [];
  for (const turn of held.turns) {
    for (const message of turn.messages) {
      const held = message as { type?: string; uuid?: string; message?: { content?: unknown } };
      if (held.type !== 'user') continue;
      const content = held.message?.content;
      const words = Array.isArray(content)
        ? content
            .map((block) => {
              const text = (block as { text?: unknown } | null)?.text;
              return typeof text === 'string' ? text : '';
            })
            .join('')
        : typeof content === 'string'
          ? content
          : '';
      out.push({ id: held.uuid ?? null, words });
    }
  }
  return out;
}

describe('a steering row across a page landing', () => {
  it('keeps its place and stays one row when the page keys it by the CLI id', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    for (const update of liveFrames()) server.update(update);

    const before = flattened(chat);
    const at = before.findIndex((row) => row.words === STEER);
    expect(at, 'the live fold drew the steering row').toBeGreaterThanOrEqual(0);

    server.send(page());

    const after = flattened(chat);
    const rows = after.filter((row) => row.words === STEER);
    expect(rows.length, 'the landing leaves exactly one row carrying the words').toBe(1);
    expect(
      after.findIndex((row) => row.words === STEER),
      'the row is at the index it was drawn at',
    ).toBe(at);
    expect(after[at - 1]?.id, 'the row before it is unchanged').toBe(before[at - 1]?.id);
    expect(after[at + 1]?.id, 'the row after it is unchanged').toBe(before[at + 1]?.id);
    stop();
  });

  it('lands a page-only steering row where its page row puts it', () => {
    // The live fold missed the delivery (a page that attached after it), so the
    // page's row is the only copy - and it belongs between the tool result it
    // followed and the thinking that came after, not at the turn's head.
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    for (const update of liveFrames()) {
      const msg = (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg as
        { uuid?: string } | undefined;
      if (msg?.uuid === 'forge_prompt_steer') continue;
      server.update(update);
    }
    server.update({ chat_appended: { key: SEAT, msg: thinking('th-2') } });
    server.update({
      chat_appended: { key: SEAT, msg: said('said-2', 'alpha beta') },
    });
    server.update({ chat_appended: { key: SEAT, msg: ended('result-2') } });

    server.send(page());

    const after = flattened(chat);
    const at = after.findIndex((row) => row.words === STEER);
    expect(at, 'the page row drew').toBeGreaterThanOrEqual(0);
    expect(after[at - 1]?.id, 'it follows the tool result it followed').toBe('result-1');
    const held = get(chat.value).turns.flatMap((turn) => turn.messages);
    const ids = held.map((message) => (message as { uuid?: string }).uuid);
    expect(ids.indexOf('77e9443d-5902-422c-85d7-6318689d1b9f')).toBe(ids.indexOf('result-1') + 1);
    expect(ids.indexOf('77e9443d-5902-422c-85d7-6318689d1b9f')).toBeLessThan(ids.indexOf('th-2'));
    stop();
  });
});
