// @vitest-environment jsdom
/**
 * A steering prompt is one row, under one id, across a page landing (#3073).
 *
 * The live path draws the delivery row when the CLI starts the prompt; a page
 * read carries the CLI's own row for it. The server puts the delivery id back
 * on that row before serving the page (`transcript::rekey_delivered`), so both
 * paths name the same message and the id-keyed merge is the whole reconcile.
 * The tests below cover the orders that broke a word-matching pairing: a page
 * read ahead of the frame stream, a replay after the landing, and a second
 * send of the same text still in the queue.
 */
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat } from './conversation';

const SEAT: SessionSlot = { org: 'Scratch', project: 'steer', label: 'lead' };
const STEER = 'STEERING-VERIFY-ROW: while that is running, also reply with the single word beta.';
const DELIVERY = 'forge_prompt_steer';
const SECOND = 'forge_prompt_second';
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

/** A queued prompt as the read carries it: the delivery id rides the block. */
const queued = (id: string, text: string): unknown => ({
  type: 'user',
  uuid: `cli-${id}`,
  message: {
    role: 'user',
    content: [{ type: 'queued_command', prompt: text, source_uuid: id }],
  },
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

const frame = (msg: unknown, source?: string): SessionUpdate => ({
  chat_appended: { key: SEAT, msg, ...(source === undefined ? {} : { source }) },
});

const snapshot = (turns: { key: string; messages: unknown[] }[]): ServerMessage =>
  ({
    kind: 'snapshot',
    subject: { session: SEAT } as Subject,
    data: {
      header: { session_id: 'scratch', turn_in_flight: false },
      conversation: { cursor: null, turns },
    },
  }) as unknown as ServerMessage;

function drawnIds(chat: Chat): string[] {
  return get(chat.value)
    .turns.flatMap((turn) => turn.messages)
    .map((message) => (message as { uuid?: string }).uuid ?? '?');
}

function steeringRows(chat: Chat): number {
  return get(chat.value)
    .turns.flatMap((turn) => turn.messages)
    .filter((message) => {
      const held = message as { type?: string; message?: { content?: unknown } };
      if (held.type !== 'user') return false;
      const content = held.message?.content;
      return Array.isArray(content)
        ? content.some((block) => (block as { text?: unknown }).text === STEER)
        : content === STEER;
    }).length;
}

describe('a steering row across a page landing', () => {
  it('pairs a page read ahead of the stream by its delivery id', () => {
    // The held fold has the display row and nothing after it; the page
    // already carries the frames the client's stream has not delivered. No
    // id here is one the held turn drew - the delivery id is the only pair.
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    server.update(frame(userText('head', 'run it'), 'you'));
    server.update(frame(userText(DELIVERY, STEER), 'you'));

    server.send(
      snapshot([
        { key: 't1', messages: [userText('head', 'run it')] },
        {
          key: 't2',
          messages: [userText(DELIVERY, STEER), thinking('th-2'), said('s-2', 'x'), ended('r-2')],
        },
      ]),
    );

    expect(steeringRows(chat), 'one prompt, one row').toBe(1);
    expect(drawnIds(chat).filter((id) => id === DELIVERY).length, 'under the delivery id').toBe(1);
    stop();
  });

  it('keeps the delivery id through a replay after the landing', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    server.update(frame(userText('head', 'run it'), 'you'));
    server.update(frame(userText(DELIVERY, STEER), 'you'));
    server.update(frame(thinking('th-2')));
    server.update(frame(said('s-2', 'x')));
    server.update(frame(ended('r-2')));
    server.send(
      snapshot([
        { key: 't1', messages: [userText('head', 'run it')] },
        {
          key: 't2',
          messages: [userText(DELIVERY, STEER), thinking('th-2'), said('s-2', 'x'), ended('r-2')],
        },
      ]),
    );

    // A repair stretch or a replayed frame for the same delivery.
    server.update(frame(userText(DELIVERY, STEER), 'you'));

    expect(steeringRows(chat), 'still one row').toBe(1);
    stop();
  });

  it('leaves the delivered row alone when a same-text send is still queued', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    server.update(frame(userText('head', 'run it'), 'you'));
    server.update(frame(userText(DELIVERY, STEER), 'you'));
    server.update(frame(thinking('th-2')));
    server.update(frame(said('s-2', 'x')));
    server.update(frame(ended('r-2')));
    server.update({
      prompt_queued: { key: SEAT, uuid: SECOND, source: 'you', text: STEER },
    });

    // The page carries the queued second send and no copy of the first: the
    // pile pulls the queued row, and the delivered row must stay put.
    server.send(
      snapshot([
        { key: 't1', messages: [userText('head', 'run it')] },
        {
          key: 't2',
          messages: [queued(SECOND, STEER), thinking('th-2'), said('s-2', 'x'), ended('r-2')],
        },
      ]),
    );

    const ids = drawnIds(chat);
    expect(ids.filter((id) => id === DELIVERY).length, 'the delivered row survives').toBe(1);
    expect(steeringRows(chat), 'and it is the only copy in the conversation').toBe(1);
    stop();
  });

  it('puts the row where the read recorded it and keeps it there', () => {
    // Live drew the row before the call already in flight; the read records
    // it after. The delivery id is shared, so the landing moves the one row
    // to the page's position and a later landing changes nothing.
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    server.update(frame(userText('forge_prompt_head', 'Run the slow command'), 'you'));
    server.update(frame(thinking('th-1')));
    server.update(frame(called('call-1-msg', 'call-1', 'Bash')));
    server.update(frame(answered('result-1', 'call-1')));
    server.update(frame(userText(DELIVERY, STEER), 'you'));
    server.update(frame(called('call-2-msg', 'call-2', 'Read')));
    server.update(frame(thinking('th-2')));
    server.update(frame(said('said-2', 'alpha beta')));
    server.update(frame(ended('result-2')));

    const before = drawnIds(chat);
    expect(before.indexOf(DELIVERY)).toBeLessThan(before.indexOf('call-2-msg'));

    const page = snapshot([
      {
        key: 't1',
        messages: [
          userText('forge_prompt_head', 'Run the slow command'),
          thinking('th-1'),
          called('call-1-msg', 'call-1', 'Bash'),
          answered('result-1', 'call-1'),
          called('call-2-msg', 'call-2', 'Read'),
        ],
      },
      {
        key: 't2',
        messages: [
          userText(DELIVERY, STEER),
          thinking('th-2'),
          said('said-2', 'alpha beta'),
          ended('result-2'),
        ],
      },
    ]);
    server.send(page);

    const after = drawnIds(chat);
    expect(steeringRows(chat), 'one row').toBe(1);
    expect(after.indexOf(DELIVERY), 'after the call the transcript recorded before it').toBe(
      after.indexOf('call-2-msg') + 1,
    );

    server.send(page);
    expect(drawnIds(chat), 'no move on a later landing').toEqual(after);
    stop();
  });

  it('lands a page-only row where its page row puts it', () => {
    // No live row at all: the page row is the only copy and the anchor keeps
    // it beside the frame its page row put it next to.
    const server = fakeConnection();
    const chat = new Chat(server.connection, SEAT);
    const stop = chat.start();
    server.update(frame(userText('forge_prompt_head', 'Run the slow command'), 'you'));
    server.update(frame(thinking('th-1')));
    server.update(frame(called('call-1-msg', 'call-1', 'Bash')));
    server.update(frame(answered('result-1', 'call-1')));
    server.update(frame(called('call-2-msg', 'call-2', 'Read')));
    server.update(frame(thinking('th-2')));
    server.update(frame(ended('result-2')));

    server.send(
      snapshot([
        {
          key: 't1',
          messages: [
            userText('forge_prompt_head', 'Run the slow command'),
            thinking('th-1'),
            called('call-1-msg', 'call-1', 'Bash'),
            answered('result-1', 'call-1'),
            called('call-2-msg', 'call-2', 'Read'),
          ],
        },
        {
          key: 't2',
          messages: [userText(CLI_STEER, STEER), thinking('th-2'), ended('result-2')],
        },
      ]),
    );

    const ids = drawnIds(chat);
    const at = ids.indexOf(CLI_STEER);
    expect(at, 'the page row drew').toBeGreaterThanOrEqual(0);
    expect(ids[at - 1], 'it follows the call its page row followed').toBe('call-2-msg');
    expect(ids[at + 1], 'it leads the thinking that came after').toBe('th-2');
    stop();
  });
});
