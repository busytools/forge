import { describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { SessionSlot } from '../wire/types';
import { StreamGate } from './stream';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

function update(seq: number | null, span = 1, text = 'x'): ServerMessage {
  return {
    kind: 'update',
    seq,
    span,
    update: {
      chat_appended: {
        key: LEAD,
        msg: { type: 'user', uuid: `u${seq}-${text}`, message: { role: 'user', content: text } },
      },
    },
  } as unknown as ServerMessage;
}

function seal(occupant: string, seq: number): ServerMessage {
  return {
    kind: 'update',
    seq,
    span: 1,
    update: {
      session_replaced: {
        key: LEAD,
        session_id: occupant,
        cwd: '/tmp',
        current_model: null,
        available_models: [],
        mode: null,
        history: [],
        dropped: 0,
        compaction_count: 0,
      },
    },
  } as unknown as ServerMessage;
}

function repaired(...seqs: number[]): ServerMessage {
  return {
    kind: 'seat_frames',
    slot: LEAD,
    epoch: null,
    frames: seqs.map((seq) => ({ seq, update: { chat_appended: { key: LEAD, msg: {} } } })),
  } as unknown as ServerMessage;
}

function snap(occupant: string, seq: number): ServerMessage {
  return {
    kind: 'snapshot',
    subject: { session: LEAD },
    data: { header: { session_id: occupant, seq } },
  } as unknown as ServerMessage;
}

function aGate(): {
  gate: StreamGate;
  asked: { from: number; to: number }[];
  emitted: ServerMessage[][];
} {
  const asked: { from: number; to: number }[] = [];
  const emitted: ServerMessage[][] = [];
  const gate = new StreamGate({
    ask: (_slot, _epoch, from, to) => {
      asked.push({ from, to });
      return true;
    },
    emit: (messages) => emitted.push(messages),
  });
  return { gate, asked, emitted };
}

describe('the stream gate', () => {
  it('passes a successor straight through and advances by its span', () => {
    const { gate } = aGate();
    gate.inbound(snap('occ', 1));
    expect(gate.inbound(update(1))).toHaveLength(1);
    expect(gate.inbound(update(2, 3))).toHaveLength(1); // stands for 2, 3, 4
    expect(gate.inbound(update(5))).toHaveLength(1);
  });

  it('holds the frames above a hole, asks for the gap, and releases in order', () => {
    const { gate, asked, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    expect(gate.inbound(update(3)), 'the frame above the hole is held').toHaveLength(0);
    expect(gate.inbound(update(4))).toHaveLength(0);
    expect(asked, 'the gap is asked for exactly').toEqual([{ from: 2, to: 2 }]);

    const released = gate.inbound(repaired(2));
    expect(released, 'the answer first, then the frames it released').toHaveLength(3);
    expect((released[0] as { kind: string }).kind).toBe('seat_frames');
    expect(released.slice(1).map((m) => (m as { seq: number }).seq)).toEqual([3, 4]);
    expect(emitted, 'nothing needed the deferred path').toHaveLength(0);
  });

  it('a second hole inside the held run gets its own ask, and the run lands in order', () => {
    const { gate, asked } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    // 3 opens the hole [2,2]; 6 opens a second one, [3,5], inside the held run.
    expect(gate.inbound(update(3)), 'the first frame above a hole waits').toHaveLength(0);
    expect(gate.inbound(update(6)), 'and so does one above a second').toHaveLength(0);
    expect(asked, 'each hole gets its own ask').toEqual([
      { from: 2, to: 2 },
      { from: 4, to: 5 },
    ]);

    const first = gate.inbound(repaired(2));
    // 2 arrives, so 3 - held all along - is contiguous with it and follows.
    expect(first.map((m) => (m as { kind: string }).kind)).toEqual(['seat_frames', 'update']);
    expect((first[1] as { seq: number }).seq, 'the held successor follows its repair').toBe(3);

    const second = gate.inbound(repaired(3, 4, 5));
    expect(second.map((m) => (m as { kind: string }).kind)).toEqual(['seat_frames', 'update']);
    const repairedSeqs = (second[0] as { frames: { seq: number }[] }).frames.map(
      (frame) => frame.seq,
    );
    expect(repairedSeqs, 'the repaired stretch lands in sequence').toEqual([4, 5]);
    expect((second[1] as { seq: number }).seq, 'and the held frame follows it').toBe(6);
  });

  it('frames that filled a hole leave nothing missing behind them', () => {
    const { gate } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    gate.inbound(update(4)); // the hole [2,3] opens
    gate.inbound(update(2)); // out of order: drains through the update path
    gate.inbound(update(3));
    // The answer says the stream restarted at 1; adopting it needs nothing
    // left missing, which the two frames above already filled.
    gate.inbound(snap('occ', 1));
    expect(gate.inbound(update(1)), 'the restarted run counts from one').toHaveLength(1);
    expect(gate.inbound(update(3)), 'and a hole in it is held').toHaveLength(0);
  });

  it('a repair for numbers already delivered is not queued again', () => {
    vi.useFakeTimers();
    const { gate, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    gate.inbound(update(4)); // the hole [2,3] opens
    const first = gate.inbound(repaired(2));
    expect((first[0] as { kind: string }).kind).toBe('seat_frames');
    const again = gate.inbound(repaired(2)); // the range answered twice
    expect(again, 'the duplicate repair has nothing to drain').toHaveLength(0);
    vi.advanceTimersByTime(5001);
    for (const batch of emitted) {
      for (const message of batch) {
        expect(
          (message as { seq?: number }).seq,
          'the duplicate repair was never handed out',
        ).not.toBe(2);
      }
    }
    vi.useRealTimers();
  });

  it('a frame behind the newest number still passes (the readers dedupe)', () => {
    const { gate } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    expect(gate.inbound(update(1))).toHaveLength(1);
  });

  it('an unnumbered frame passes untouched and uncounted', () => {
    const { gate } = aGate();
    gate.inbound(snap('occ', 1));
    expect(gate.inbound(update(null))).toHaveLength(1);
    expect(gate.inbound(update(null))).toHaveLength(1);
    // Still expecting 1: the unnumbered frames consumed no numbers.
    expect(gate.inbound(update(1))).toHaveLength(1);
  });

  it('a seal under the same occupant also restarts the count', () => {
    const { gate, asked, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(seal('occ', 1));
    gate.inbound(update(1));
    gate.inbound(update(2, 5)); // stands for 2..6; expecting is 7
    // The same occupant seals again - a restart, which the server also mints
    // from 1 - and the gate must count from 1 with it.
    gate.inbound(seal('occ', 7));
    expect(gate.inbound(update(1)), 'the restarted run counts from one').toHaveLength(1);
    expect(gate.inbound(update(3)), 'and a hole in it is held').toHaveLength(0);
    expect(asked, 'the hole in the restarted run is asked for').toEqual([{ from: 2, to: 2 }]);
    expect(emitted, 'nothing released early').toHaveLength(0);
  });

  it('a snapshot lowers the count when the stream has restarted', () => {
    const { gate } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    gate.inbound(update(2));
    const restarted = {
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { session_id: 'occupant', seq: 1 } },
    } as unknown as ServerMessage;
    gate.inbound(restarted);
    // Expecting 1 again: a frame at 1 is a successor, and one at 3 is a hole.
    expect(gate.inbound(update(1))).toHaveLength(1);
    expect(gate.inbound(update(3))).toHaveLength(0);
  });

  it('a seal passes at once and starts the count over', () => {
    const { gate, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    gate.inbound(update(3)); // held, asking
    const sealFrame = seal('new-occupant', 4);
    expect(gate.inbound(sealFrame), 'the seal goes out at once').toHaveLength(1);
    // The hold died with the old epoch, and the new count is 1.
    expect(emitted).toHaveLength(0);
    expect(gate.inbound(update(1))).toHaveLength(1);
  });

  it('a refusal clears the hold and the readers reread', () => {
    const { gate, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    expect(gate.inbound(update(4))).toHaveLength(0);
    const refusal = {
      kind: 'error',
      what: 'seat_range',
      why: "the seat's occupant has changed",
      seat: LEAD,
    } as unknown as ServerMessage;
    expect(gate.inbound(refusal)).toHaveLength(1);
    expect(emitted, 'a refused hold is dropped, not released').toHaveLength(0);
  });

  it('releases the hold when the cap is reached', () => {
    const { gate, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    for (let n = 3; n < 3 + 256; n += 1) gate.inbound(update(n));
    expect(emitted, 'the cap released the frames').toHaveLength(1);
    const released = emitted[0] ?? [];
    expect(released[0]).toMatchObject({ seq: 3 });
  });

  it('releases the hold when the socket cannot ask', () => {
    const emitted: ServerMessage[][] = [];
    const gate = new StreamGate({ ask: () => false, emit: (m) => emitted.push(m) });
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    expect(gate.inbound(update(3))).toHaveLength(0);
    expect(emitted, 'no answer is coming, so nothing waits').toHaveLength(1);
  });

  it('releases the hold when the answer never comes', () => {
    vi.useFakeTimers();
    const { gate, emitted } = aGate();
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    gate.inbound(update(3));
    vi.advanceTimersByTime(5001);
    expect(emitted, 'the deadline released the frames').toHaveLength(1);
    vi.useRealTimers();
  });

  it('answers a snapshot by adopting its header', () => {
    const { gate } = aGate();
    const snapshot = {
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { session_id: 'occupant', seq: 5 } },
    } as unknown as ServerMessage;
    gate.inbound(snapshot);
    // The answer names the next number: 5. A frame at 5 is a successor.
    expect(gate.inbound(update(5))).toHaveLength(1);
  });

  it('a seat nothing has vouched for passes through, and asks for nothing', () => {
    const { gate, asked } = aGate();
    // Fleet news riding a coarser watch (the home's): numbered on a stream
    // this connection never subscribed to. Asking for the stretch below it
    // demands a whole history nobody opened - and an awake seat refuses the
    // ask outright, which reappears on the next fleet frame and loops
    // (measured live, 2026-10-15).
    expect(gate.inbound(update(6957))).toHaveLength(1);
    expect(asked, 'a seat nothing follows opens no ask').toEqual([]);
    // Once a snapshot vouches for the seat, its numbers are held as usual.
    gate.inbound(snap('occ', 1));
    gate.inbound(update(1));
    expect(gate.inbound(update(3)), 'a hole in a followed seat is still held').toHaveLength(0);
    expect(asked, 'and it is asked for').toEqual([{ from: 2, to: 2 }]);
  });
});
