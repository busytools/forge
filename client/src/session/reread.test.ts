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
import { connect, MAX_RETRY_MS } from '../socket';
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
  // **A `::` listener can share a port with a v4 one**, and the page's v4 dial
  // then lands on that process; bind the address the page dials.
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;
  const sockets = new Set<import('ws').WebSocket>();
  const timers: ReturnType<typeof setTimeout>[] = [];
  const subscribes: Subject[] = [];
  /** Every frame this forge emitted, for a repair ask to answer from. */
  const sent = new Map<number, SessionUpdate>();
  /** The row each frame carries, for a snapshot to include what it has sent. */
  const rows = new Map<number, unknown>();
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
        // **The answer carries the rows this forge had streamed by its encode
        // point**, as the real record does. Without them a taken answer would
        // drop rows the client had already drawn.
        const carried: unknown[] = [];
        for (let at = 1; at <= reach; at += 1) {
          const row = rows.get(at);
          if (row !== undefined) carried.push(row);
        }
        const body =
          subjectKey(what) === subjectKey({ session: LEAD })
            ? {
                ...sessionFixture,
                slot: LEAD,
                conversation: {
                  ...sessionFixture.conversation,
                  turns:
                    carried.length > 0
                      ? [
                          ...sessionFixture.conversation.turns,
                          { key: 'streamed', messages: carried },
                        ]
                      : sessionFixture.conversation.turns,
                },
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
    const msg = spoke(`u${seq}`);
    rows.set(seq, msg);
    const update: SessionUpdate = { chat_appended: { key: LEAD, msg } };
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
    /** How many frames this forge has emitted so far. */
    emitted: () => seq,
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

/** The streamed rows a record carries, in the order they reached it. */
function streamed(read: SessionRead): string[] {
  return spoken(read).filter((word) => /^u\d+$/.test(word));
}

const wait = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * How long a wait here is given: a read crosses a real socket and can ride the
 * client's own reconnect ceiling.
 */
const WAIT_MS = MAX_RETRY_MS + 3_000;

async function until(what: () => boolean, why: string, deadlineMs = WAIT_MS): Promise<void> {
  const deadline = Date.now() + deadlineMs;
  while (!what()) {
    if (Date.now() > deadline) throw new Error(`the wait ran out: ${why}`);
    await wait(5);
  }
}

/**
 * The slow-link seat, open and drawing: the rig both bound cases share.
 *
 * The seat's answer is ten cadences away and the seat keeps streaming, so the
 * frames outrun it by construction - the shape #2561, #2563 and #2564 all
 * live in.
 */
async function atBound() {
  const server = await slowLink(ANSWER_MS, 15);
  const connection = connect(server.url, BOUND_MS);
  const view = watchSession(connection, LEAD, true);
  // A reader is what makes the store show the seat, and its subscription is
  // what opens the seat's own.
  const stop = view.subscribe(() => undefined);
  return {
    server,
    view,
    close: () => {
      stop();
      connection.close();
      server.stop();
    },
  };
}

/**
 * **A slow first record must not draw the repaired stretch twice** (#2564).
 */
describe('a first whole record slower than the frames', () => {
  it('draws every streamed row once', async () => {
    const rig = await atBound();
    try {
      await until(() => get(rig.view).wire !== null, 'the seat to draw its record');
      await wait(QUIET_MS);

      const emitted = rig.server.emitted();
      await until(
        () => streamed(get(rig.view)).length >= emitted,
        'the emitted frames to reach the record',
      );
      const words = streamed(get(rig.view));
      expect(
        words,
        'the record is not u1..uN once each: a duplicate, a hole, or a row out of turn',
      ).toEqual(Array.from({ length: words.length }, (_, at) => `u${at + 1}`));
    } finally {
      rig.close();
    }
  });
});

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
    const rig = await atBound();

    try {
      // The first answer lands and the seat draws its record.
      await until(() => get(rig.view).wire !== null, 'the seat to draw its record');

      // And every ask the answer's own arrival could have bought has had its
      // window.
      await wait(QUIET_MS);

      // **The bound's one retry went, and nothing else did.** The lower bound
      // is the retry at five seconds (a bound that never fired would leave the
      // one subscribe the page made). The old ceiling allowed the read, its
      // retry, and the outrun path's up to three re-asks; with the outrun
      // answer dropped rather than re-read (#2563), the retry is the last
      // ask. The cadence amplifier measured thirteen in this window.
      const asked = rig.server.seatSubscribes().length;
      expect(asked, 'the read was either not retried or re-read again past the bound').toBe(2);

      // **The frames that crossed the answer reach the record, whole and in
      // order.** The count is taken from the forge and the record is waited
      // on, so a frame emitted while this runs only lengthens the tail and the
      // check is race-safe. Without the repair the carried rows stop at the
      // answer's encode point and the record never catches up to the count.
      const emitted = rig.server.emitted();
      await until(
        () => streamed(get(rig.view)).length >= emitted,
        'the emitted frames to reach the record',
      );
      const words = streamed(get(rig.view));
      expect(words, 'the repaired stretch has a duplicate, a hole, or a row out of turn').toEqual(
        Array.from({ length: words.length }, (_, at) => `u${at + 1}`),
      );
      expect(
        words.length,
        "the rows above the answer's encode point never reached the record",
      ).toBeGreaterThanOrEqual(emitted);
    } finally {
      rig.close();
    }
  });
});
