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
    expect(gate.inbound(update(1))).toHaveLength(1);
    expect(gate.inbound(update(2, 3))).toHaveLength(1); // stands for 2, 3, 4
    expect(gate.inbound(update(5))).toHaveLength(1);
  });

  it('holds the frames above a hole, asks for the gap, and releases in order', () => {
    const { gate, asked, emitted } = aGate();
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

  it('a frame behind the newest number still passes (the readers dedupe)', () => {
    const { gate } = aGate();
    gate.inbound(update(1));
    expect(gate.inbound(update(1))).toHaveLength(1);
  });

  it('an unnumbered frame passes untouched and uncounted', () => {
    const { gate } = aGate();
    expect(gate.inbound(update(null))).toHaveLength(1);
    expect(gate.inbound(update(null))).toHaveLength(1);
    // Still expecting 1: the unnumbered frames consumed no numbers.
    expect(gate.inbound(update(1))).toHaveLength(1);
  });

  it('a seal passes at once and starts the count over', () => {
    const { gate, emitted } = aGate();
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
    gate.inbound(update(1));
    for (let n = 3; n < 3 + 256; n += 1) gate.inbound(update(n));
    expect(emitted, 'the cap released the frames').toHaveLength(1);
    const released = emitted[0] ?? [];
    expect(released[0]).toMatchObject({ seq: 3 });
  });

  it('releases the hold when the socket cannot ask', () => {
    const emitted: ServerMessage[][] = [];
    const gate = new StreamGate({ ask: () => false, emit: (m) => emitted.push(m) });
    gate.inbound(update(1));
    expect(gate.inbound(update(3))).toHaveLength(0);
    expect(emitted, 'no answer is coming, so nothing waits').toHaveLength(1);
  });

  it('releases the hold when the answer never comes', () => {
    vi.useFakeTimers();
    const { gate, emitted } = aGate();
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
});
