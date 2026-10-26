// Bare, because this file runs in the node environment: it drives the real
// connection over a real socket, and under jsdom the same import lands on ws's
// browser shim, which throws.
import { type AddressInfo, type RawData, WebSocketServer } from 'ws';
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { connect, MAX_RETRY_MS, type Connection } from '../socket';
import { watchSession } from '../session/live';
import type { SessionSlot } from '../wire/types';
import { Chat } from './conversation';
import { fold } from './units';

const SEAT: SessionSlot = { org: 'Bench', project: 'seat', label: 'lead' };
const ENDING = 'Agent "Review #3938 dependency security" finished';

/** The CLI's own ending for the call, as both carriers spell it. */
const ENVELOPE =
  '<task-notification>\n<task-id>a9498c470d06d0bae</task-id>\n' +
  '<tool-use-id>toolu_01TMh27hpntyU8qR6WY2aEsZ</tool-use-id>\n<status>completed</status>\n' +
  '<summary>Agent "Review #3938 dependency security" finished</summary>\n</task-notification>';

/** A settled assistant row, which the re-cut page splits off the call. */
const opening = {
  type: 'assistant',
  uuid: 'eac30f7d-e121-4a75-acce-9055cbc6eb0c',
  timestamp: '2026-10-21T12:32:21.062Z',
  message: {
    id: 'msg_1',
    role: 'assistant',
    model: 'claude-opus-5-5',
    content: [{ type: 'text', text: 'The draft PR is up.' }],
  },
};

/** The call the ending belongs to. */
const call = {
  type: 'assistant',
  uuid: '2efdd1fa-2154-40d6-856b-aa70d4460194',
  timestamp: '2026-10-21T12:31:53.555Z',
  message: {
    id: 'msg_2',
    role: 'assistant',
    model: 'claude-opus-5-5',
    content: [
      {
        type: 'tool_use',
        id: 'toolu_01TMh27hpntyU8qR6WY2aEsZ',
        name: 'Agent',
        input: {
          description: 'Review #3938 dependency security',
          subagent_type: 'pensive:rust-auditor',
        },
      },
    ],
  },
};

/**
 * The copy the server forges into the call's turn, so a per-turn fold can
 * close the call: no id, because the CLI's own row owns one.
 */
const forged = {
  type: 'user',
  message: {
    role: 'user',
    content: [{ type: 'queued_command', commandMode: 'task-notification', prompt: ENVELOPE }],
  },
};

/** The CLI's own row for the ending, which is the copy that draws. */
const carrier = {
  type: 'user',
  uuid: '981573f2-48f3-4e24-9ebd-d3dfad5a537f',
  timestamp: '2026-10-21T12:39:14.462Z',
  message: {
    role: 'user',
    content: [
      {
        type: 'queued_command',
        commandMode: 'task-notification',
        prompt: ENVELOPE,
        source_uuid: '899dbf85-acc7-4ade-a41b-4bf75bc8f753',
      },
    ],
  },
};

/** The page a seat loads with: the call's turn and the row's own turn. */
const loaded = {
  cursor: '1',
  turns: [
    { key: '2026-10-21T09:19:52.497Z', messages: [opening, call, forged] },
    { key: '2026-10-21T12:39:05.390Z', messages: [carrier] },
  ],
};

/**
 * The same conversation cut differently on the next read, which is what a
 * transcript that kept growing does: the call's turn no longer opens where it
 * did, so the merge finds it by the frame it shares rather than by its name.
 */
const recut = {
  cursor: '2',
  turns: [
    { key: '2026-10-21T09:19:52.497Z', messages: [opening] },
    { key: '2026-10-21T12:31:53.555Z', messages: [call, forged] },
    { key: '2026-10-21T12:39:05.390Z', messages: [carrier] },
  ],
};

/** How many ending rows the conversation draws, over every turn it holds. */
function drawn(chat: Chat, text = ENDING): number {
  return get(chat.value).turns.reduce(
    (count, turn) =>
      count +
      fold(turn.messages).filter((unit) => unit.kind === 'notice' && unit.notice.text === text)
        .length,
    0,
  );
}

/** Every call row's own status, in the order the conversation draws them. */
function statuses(chat: Chat): string[] {
  const out: string[] = [];
  for (const turn of get(chat.value).turns) {
    for (const unit of fold(turn.messages)) {
      if (unit.kind !== 'leaves') continue;
      for (const row of unit.rows) {
        if (row.tag === 'call') out.push(row.leaf.status);
      }
    }
  }
  return out;
}

/** One frame as text. Everything here is JSON, which `ws` hands over as a Buffer. */
function frame(data: RawData): string {
  if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('utf8');
  return data.toString('utf8');
}

/** A forge that answers the conversation's pages from whatever it is given. */
async function stubForge() {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;

  let page: unknown = { turns: [], cursor: null };
  let opened = 0;
  let sockets: import('ws').WebSocket[] = [];

  const answer = (socket: import('ws').WebSocket, message: Record<string, unknown>): void => {
    const held = page as { turns: unknown[]; cursor: unknown };
    if (message.kind === 'subscribe') {
      socket.send(
        JSON.stringify({
          kind: 'snapshot',
          subject: message.what,
          data: {
            slot: SEAT,
            header: { session_id: 'sess-1', seq: 1, turn_in_flight: false },
            state: { queue: [] },
            conversation: held,
          },
        }),
      );
    }
    if (message.kind === 'more') {
      socket.send(
        JSON.stringify({
          kind: 'page',
          conversation: SEAT,
          turns: held.turns,
          cursor: held.cursor,
        }),
      );
    }
  };

  server.on('connection', (socket) => {
    opened += 1;
    sockets.push(socket);
    socket.on('close', () => {
      sockets = sockets.filter((held) => held !== socket);
    });
    socket.on('message', (data: RawData) => {
      answer(socket, JSON.parse(frame(data)) as Record<string, unknown>);
    });
  });

  return {
    url: `ws://127.0.0.1:${port}/socket`,
    serve(next: unknown) {
      page = next;
    },
    opened: () => opened,
    drop() {
      for (const socket of sockets) socket.terminate();
      sockets = [];
    },
    async close() {
      for (const socket of sockets) socket.terminate();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

/**
 * How long a wait here is given: a page crossing a real socket can ride the
 * client's own reconnect ceiling, so the budget is that plus the span the
 * page's own parse needs.
 */
const WAIT_MS = MAX_RETRY_MS + 3_000;

// A case here can carry five waits in sequence, so the file's own deadline is
// that sum rather than vitest's five-second default, which is one wait's
// budget and leaves a chained case to red on a bare timeout.
vi.setConfig({ testTimeout: WAIT_MS * 5 + 5_000 });

async function waitFor(what: string, predicate: () => boolean, ms = WAIT_MS): Promise<void> {
  const until = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > until) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

/** The conversation, once a page has landed, counted by its own page handle. */
function pages(chat: Chat): number {
  return get(chat.value).pages;
}

/** Every frame the conversation holds, over every turn. */
function frames(chat: Chat): number {
  return get(chat.value).turns.reduce((count, turn) => count + turn.messages.length, 0);
}

/** What this file opened, closed after each test whether or not it passed. */
const forges: Awaited<ReturnType<typeof stubForge>>[] = [];
const connections: Connection[] = [];

describe('a task ending over repeated reads', () => {
  afterEach(async () => {
    for (const connection of connections.splice(0)) connection.close();
    for (const forge of forges.splice(0)) await forge.close();
    vi.unstubAllGlobals();
  });

  it('draws one row however many pages the conversation takes', async () => {
    // A page paints on the frame; a shim is the deterministic one.
    vi.stubGlobal('requestAnimationFrame', (fn: (at: number) => void) => setTimeout(fn, 0));
    vi.stubGlobal('cancelAnimationFrame', (handle: number) => clearTimeout(handle));

    const forge = await stubForge();
    forges.push(forge);
    forge.serve(loaded);
    const connection: Connection = connect(forge.url);
    connections.push(connection);
    const chat = new Chat(connection, SEAT);
    chat.start();
    const hold = watchSession(connection, SEAT, true);
    hold.subscribe(() => undefined);

    await waitFor('the load', () => pages(chat) >= 1);
    expect(drawn(chat), 'the ending draws once when the seat loads').toBe(1);
    const loadedFrames = frames(chat);

    // The restart: the socket drops, the seat is asked again, and the page it
    // gets is the same conversation cut at different turns.
    forge.serve(recut);
    forge.drop();
    await waitFor('the reconnect', () => forge.opened() >= 2);
    await waitFor('the reconnect page', () => pages(chat) >= 2);
    expect(drawn(chat), 'and still once over the page the reconnect lands').toBe(1);
    expect(frames(chat), 'the re-cut page adds no copy of the ending it holds').toBe(loadedFrames);

    // A return lands the window again, which is the other way a page repeats.
    forge.serve(loaded);
    forge.drop();
    await waitFor('the second reconnect', () => forge.opened() >= 3);
    await waitFor('the return page', () => pages(chat) >= 3);
    expect(drawn(chat), 'and once over a page that repeats what is held').toBe(1);
    expect(frames(chat), 'the repeated page adds no copy of the ending it holds').toBe(
      loadedFrames,
    );
  });

  it('keeps two endings that say the same words apart', async () => {
    vi.stubGlobal('requestAnimationFrame', (fn: (at: number) => void) => setTimeout(fn, 0));
    vi.stubGlobal('cancelAnimationFrame', (handle: number) => clearTimeout(handle));

    // Two reviews described the same way: their notices say the same words and
    // only the envelope tells them apart.
    const says = 'Agent "Review" finished';
    const envelope = (task: string, call: string): string =>
      '<task-notification>\n<task-id>' +
      task +
      '</task-id>\n<tool-use-id>' +
      call +
      '</tool-use-id>\n<status>completed</status>\n<summary>' +
      says +
      '</summary>\n</task-notification>';
    const reviewed = (uuid: string, call: string): unknown => ({
      type: 'assistant',
      uuid,
      message: {
        id: `msg-${call}`,
        role: 'assistant',
        model: 'claude-opus-5-5',
        content: [
          {
            type: 'tool_use',
            id: call,
            name: 'Agent',
            input: { description: 'Review', subagent_type: 'pensive:rust-auditor' },
          },
        ],
      },
    });
    const ended = (prompt: string, source: string): unknown => ({
      type: 'user',
      uuid: `row-${source}`,
      message: {
        role: 'user',
        content: [
          {
            type: 'queued_command',
            commandMode: 'task-notification',
            prompt,
            source_uuid: source,
          },
        ],
      },
    });
    const callA = reviewed('call-a', 'toolu_a');
    const callB = reviewed('call-b', 'toolu_b');
    const forgedA = {
      type: 'user',
      message: {
        role: 'user',
        content: [
          {
            type: 'queued_command',
            commandMode: 'task-notification',
            prompt: envelope('task-a', 'toolu_a'),
          },
        ],
      },
    };
    const forgedB = {
      type: 'user',
      message: {
        role: 'user',
        content: [
          {
            type: 'queued_command',
            commandMode: 'task-notification',
            prompt: envelope('task-b', 'toolu_b'),
          },
        ],
      },
    };
    const carrierA = ended(envelope('task-a', 'toolu_a'), 'source-a');
    const carrierB = ended(envelope('task-b', 'toolu_b'), 'source-b');

    const forge = await stubForge();
    forges.push(forge);
    forge.serve({
      cursor: '1',
      turns: [
        { key: 'call-a', messages: [callA, forgedA] },
        { key: 'rows-a', messages: [carrierA] },
      ],
    });
    const connection: Connection = connect(forge.url);
    connections.push(connection);
    const chat = new Chat(connection, SEAT);
    chat.start();
    const hold = watchSession(connection, SEAT, true);
    hold.subscribe(() => undefined);

    await waitFor('the load', () => pages(chat) >= 1);
    expect(drawn(chat, says), 'the first ending draws once').toBe(1);

    // The same turn, re-read after the transcript grew: it now holds the
    // second call and its ending, which says the same words as the first, and
    // the first ending's own row has moved out of it.
    forge.serve({
      cursor: '2',
      turns: [
        { key: 'call-a', messages: [callA, callB, forgedB] },
        { key: 'rows', messages: [carrierA, carrierB] },
      ],
    });
    forge.drop();
    await waitFor('the reconnect', () => forge.opened() >= 2);
    await waitFor('the page', () => pages(chat) >= 2);
    expect(drawn(chat, says), 'both endings draw, one row each').toBe(2);
    expect(statuses(chat), 'and both calls read finished').toEqual(['completed', 'completed']);
  });
});
