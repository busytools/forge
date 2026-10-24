// @vitest-environment jsdom
import { createRequire } from 'node:module';

import { get } from 'svelte/store';
import type { AddressInfo, RawData, WebSocketServer as Server } from 'ws';
import { describe, expect, it, vi } from 'vitest';

/**
 * `ws` by its CommonJS entry, as `live.test.ts` does: this file runs in jsdom,
 * and the package's `browser` field resolves the import to a client-only shim
 * with no server in it.
 */
const { WebSocketServer, WebSocket: ClientSocket } = createRequire(import.meta.url)('ws') as {
  WebSocketServer: typeof Server;
  WebSocket: unknown;
};
(globalThis as { WebSocket?: unknown }).WebSocket = ClientSocket;

import sessionFixture from '../dev/fixtures/session.json';
import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import { PROTOCOL_VERSION, subjectKey } from '../protocol';
import { connect } from '../socket';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import { watchSession, type SessionRead } from './live';

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'handoff-cut' };

/**
 * A whole-record answer's travel, well past the bound: the shape #2561 was
 * filed on, where a 2.4-3.7 MB snapshot took 2-18 s to write down a slow link.
 * The figures are scaled so a test reads in a second, not a minute.
 */
const ANSWER_MS = 400;
const BOUND_MS = 40;

/** How long a claim that nothing was sent is held open for a frame to arrive. */
const QUIET_MS = 150;

vi.setConfig({ testTimeout: 20_000 });

function text(data: RawData): string {
  if (typeof data === 'string') return data;
  if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
  return Buffer.from(data as ArrayBuffer).toString('utf8');
}

/** One user row, as the CLI reports it. */
function spoke(uuid: string): Record<string, unknown> {
  return {
    type: 'user',
    uuid,
    message: { role: 'user', content: [{ type: 'text', text: uuid }] },
    session_id: 'occ-1',
  };
}

/**
 * A forge on a slow link: the seat's whole record is answered only after
 * `ANSWER_MS` while the seat keeps streaming, and every subscribe is counted.
 *
 * **The encode happens when the ask is read**, not when the answer leaves: the
 * snapshot's number is where the stream stood at the ask. So the frames minted
 * while it travels are already on the client when the answer lands - the
 * "answer behind the stream" this file exists to hold.
 */
async function slowLink(answerMs: number, frameMs: number) {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;
  const sockets = new Set<import('ws').WebSocket>();
  const timers: ReturnType<typeof setTimeout>[] = [];
  const subscribes: Subject[] = [];
  /** Every frame this forge emitted, for a repair ask to answer from. */
  const sent = new Map<number, SessionUpdate>();
  let seq = 0;

  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    socket.send(
      JSON.stringify({
        kind: 'greeting',
        version: PROTOCOL_VERSION,
        settings: DEFAULT_SETTINGS,
      } satisfies ServerMessage),
    );
    socket.on('message', (data) => {
      const message = JSON.parse(text(data)) as {
        kind: string;
        what?: Subject;
        slot?: SessionSlot;
        from?: number;
        to?: number;
      };
      // **A repair is answered from what this forge emitted**, as the real
      // server answers from its ring: the frames a hole holds are not lost.
      if (message.kind === 'seat_range' && message.slot !== undefined) {
        const from = message.from ?? 0;
        const to = message.to ?? 0;
        const frames = [];
        for (let at = from; at <= to; at += 1) {
          const update = sent.get(at);
          if (update !== undefined) frames.push({ seq: at, update });
        }
        socket.send(
          JSON.stringify({
            kind: 'seat_frames',
            slot: message.slot,
            epoch: 'occ-1',
            frames,
          } satisfies ServerMessage),
        );
        return;
      }
      const what = message.what;
      if (message.kind !== 'subscribe' || what === undefined) return;
      subscribes.push(what);
      const reach = seq;
      const answer = setTimeout(() => {
        const body =
          subjectKey(what) === subjectKey({ session: LEAD })
            ? {
                ...sessionFixture,
                slot: LEAD,
                header: { ...sessionFixture.header, session_id: 'occ-1', seq: reach + 1 },
              }
            : {};
        socket.send(
          JSON.stringify({
            kind: 'snapshot',
            subject: what,
            data: body,
          } satisfies ServerMessage),
        );
      }, answerMs);
      timers.push(answer);
    });
  });

  // The seat keeps streaming while its answer is on the way.
  const beat = setInterval(() => {
    seq += 1;
    const update: SessionUpdate = { chat_appended: { key: LEAD, msg: spoke(`u${seq}`) } };
    sent.set(seq, update);
    for (const socket of sockets) {
      socket.send(JSON.stringify({ kind: 'update', update, seq, span: 1 } satisfies ServerMessage));
    }
  }, frameMs);

  return {
    url: `ws://127.0.0.1:${port}/socket`,
    /** Every subscribe this forge was sent, in order. */
    subscribes,
    seatSubscribes: () =>
      subscribes.filter((what) => subjectKey(what) === subjectKey({ session: LEAD })),
    stop: () => {
      clearInterval(beat);
      for (const timer of timers) clearTimeout(timer);
      for (const socket of sockets) socket.terminate();
      server.close();
    },
  };
}

/** Every word a record's turns carry, in the order its frames arrived. */
function spoken(read: SessionRead): string[] {
  return (read.wire?.conversation.turns ?? []).flatMap((turn) =>
    turn.messages.flatMap((frame) => {
      const content = (frame as { message?: { content?: { text?: string }[] } }).message?.content;
      return (content ?? []).map((part) => part.text ?? '');
    }),
  );
}

const wait = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

async function until(what: () => boolean, deadlineMs = 5_000): Promise<void> {
  const deadline = Date.now() + deadlineMs;
  while (!what()) {
    if (Date.now() > deadline) throw new Error('the wait ran out');
    await wait(5);
  }
}

/**
 * **A slow whole-record answer must not buy whole re-reads** (#2561).
 *
 * The seat's answer is ten cadences away and the seat keeps streaming: a bound
 * that re-asks on every cadence sends ten fresh subscribes, each one a whole
 * encode and 2.4-3.7 MB on the same socket, queued behind the answer already
 * in flight - the amplifier that held a WireGuard tunnel at 0.13-0.34 MB/s.
 * The read is given its retry (a lost snapshot is real), and past it the
 * answer is presumed slow rather than lost.
 */
describe('a whole-record answer slower than the bound', () => {
  it('is re-asked once, not on every cadence', async () => {
    const server = await slowLink(ANSWER_MS, 15);
    const connection = connect(server.url, BOUND_MS);
    const view = watchSession(connection, LEAD, true);
    // A reader is what makes the store show the seat, and its subscription is
    // what opens the seat's own.
    const stop = view.subscribe(() => undefined);

    try {
      // The first answer lands and the seat draws its record.
      await until(() => get(view).wire !== null);
      // And the retry's own answer, and any frames after it, have had their
      // window: nothing more is owed.
      await wait(QUIET_MS);

      // **The ceiling is the one-shot paths, not the cadence**: the read
      // itself, its one bound retry, and the outrun path's few re-asks for an
      // answer that lands behind the frames. The cadence amplifier measured
      // thirteen within this window.
      expect(
        server.seatSubscribes().length,
        'a slow answer bought a whole re-read on every bound cadence',
      ).toBeLessThanOrEqual(5);
      expect(
        spoken(get(view)),
        'the frames that streamed while the answer travelled were dropped',
      ).toContain('u1');
    } finally {
      stop();
      connection.close();
      server.stop();
    }
  });
});
