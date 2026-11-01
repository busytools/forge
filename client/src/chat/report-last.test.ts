import { describe, expect, it } from 'vitest';

import { fold, turnFold, type InboundLeaf, type PeerCard, type Unit } from './units';

/**
 * Where the turn's report row sits when a message arrives after the
 * assistant's last output (#2766).
 *
 * The transcript keeps a peer envelope in the turn it interrupted - a
 * bracket-led row opens no turn - so the fold is what says the turn ended: the
 * closing line draws before the peer row, and a peer message with assistant
 * work after it was mid-turn and keeps the place it landed in.
 *
 * The running row is the same row, one frame earlier, so both paths read the
 * same way.
 */

/** One assistant message carrying prose. */
const said = (value: string): unknown => ({
  type: 'assistant',
  uuid: `a-${value}`,
  message: {
    id: `m-${value}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text: value }],
  },
});

/** A peer message, as `peers/types.rs`'s `to_prose` writes it. */
const peer = (id: string, body: string): unknown => ({
  type: 'user',
  uuid: `u-${id}`,
  message: {
    role: 'user',
    content: [
      {
        type: 'text',
        text: `[Message id=${id} from agent 'forge/backlog-browser' (org 'Busytools')]\n\n${body}`,
      },
    ],
  },
});

/** A Slack delivery, as `delivery.rs` writes the prose it carries. */
const slack = (body: string): unknown => ({
  type: 'user',
  uuid: 'u-slack',
  message: {
    role: 'user',
    content: [
      {
        type: 'text',
        text: `[Slack - workspace 'Busytools', #forge] id C1 ts 1.2\nsteward: ${body}`,
      },
    ],
  },
});

/** One assistant message carrying a call, so the message has work above it. */
const called = (id: string): unknown => ({
  type: 'assistant',
  uuid: `a-${id}`,
  message: {
    id: `m-${id}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'tool_use', id, name: 'Bash', input: { command: 'just check' } }],
  },
});

/** The frame a turn ends on. */
const ended = (): unknown => ({
  type: 'result',
  uuid: 'r-1',
  subtype: 'success',
  is_error: false,
  duration_ms: 1000,
  usage: { input_tokens: 10, output_tokens: 2 },
});

/** A counter frame, which draws nothing and so may be carried rather than re-folded. */
const counted = (delta: number): unknown => ({
  type: 'system',
  subtype: 'thinking_tokens',
  estimated_tokens_delta: delta,
  uuid: `t-${delta}`,
});

const kinds = (units: Unit[]): string[] => units.map((unit) => unit.kind);

/** The peer cards a fold drew, in the order they drew. */
const cards = (units: Unit[]): PeerCard[] =>
  units.flatMap((unit) =>
    unit.kind === 'leaves'
      ? unit.rows.flatMap((row) => (row.tag === 'card' ? [row.card] : []))
      : [],
  );

/** The delivery rows a fold drew, in the order they drew. */
const inbounds = (units: Unit[]): InboundLeaf[] =>
  units.flatMap((unit) =>
    unit.kind === 'leaves' ? unit.rows.flatMap((row) => (row.tag === 'inbound' ? [row] : [])) : [],
  );

describe('the report row closes the turn', () => {
  it('draws the closing line before a peer message that arrived after the last output', () => {
    const units = fold([said('the fix is in'), peer('m-1', 'take a look'), ended()]);

    expect(kinds(units), 'the report last, the peer row after it').toEqual([
      'text',
      'report',
      'leaves',
    ]);
    expect(
      cards(units).map((card) => card.body),
      'the message still draws, after the line that ended the turn',
    ).toEqual(['take a look']);
    expect(
      units[1]?.kind === 'report' ? units[1].info.running : true,
      'the settled row, not a running one',
    ).toBe(false);
  });

  it('draws a plain message that arrived after the last output below the closing line', () => {
    const typed = {
      type: 'user',
      uuid: 'u-2',
      message: { role: 'user', content: [{ type: 'text', text: 'one more thing' }] },
    };

    expect(kinds(fold([said('the fix is in'), typed, ended()]))).toEqual([
      'text',
      'report',
      'user',
    ]);
  });

  it('keeps the call a message arrived above on its side of the closing line', () => {
    // The call is still pending when the message lands, so the two share one
    // work unit: the row splits, or the call draws after the line that ended
    // the turn.
    const units = fold([called('c-1'), peer('m-1', 'take a look'), ended()]);

    expect(kinds(units), 'the call above the line, the message below it').toEqual([
      'leaves',
      'report',
      'leaves',
    ]);
    expect(
      cards(units).map((card) => card.body),
      'and the message still draws',
    ).toEqual(['take a look']);
  });

  it("draws a delivery that arrived at the turn's end below the closing line", () => {
    const units = fold([said('the fix is in'), slack('the gate is green'), ended()]);

    expect(kinds(units), 'the report closes the turn, the delivery after it').toEqual([
      'text',
      'report',
      'leaves',
    ]);
    expect(
      inbounds(units).map((row) => row.body),
      'the delivery still draws',
    ).toEqual(['the gate is green']);
  });

  it('keeps a peer message with a call after it above the closing line', () => {
    // The call is the assistant's last output, and it came after the message -
    // so the message was mid-turn and the tail is not a run of messages.
    const units = fold([said('the fix is in'), peer('m-1', 'take a look'), called('c-1'), ended()]);

    expect(kinds(units), 'the message keeps the place it landed in').toEqual([
      'text',
      'leaves',
      'report',
    ]);
    expect(
      cards(units).map((card) => card.body),
      'and the message still draws',
    ).toEqual(['take a look']);
  });

  it('keeps a peer message that arrived before the last output where it landed', () => {
    expect(kinds(fold([peer('m-1', 'take a look'), said('the fix is in'), ended()]))).toEqual([
      'leaves',
      'text',
      'report',
    ]);
  });

  it('places the running row the same way, and the carry keeps it there', () => {
    // The running row is the settled row one frame earlier: a peer message that
    // lands after the assistant's last output already draws below it, so the
    // row does not jump above the message when the result lands.
    let turn: { messages: readonly unknown[] } = { messages: [said('working')] };
    expect(kinds(turnFold(turn, null, true, false)), 'the row under the work').toEqual([
      'text',
      'report',
    ]);

    turn = { messages: [...turn.messages, peer('m-1', 'take a look')] };
    expect(kinds(turnFold(turn, null, true, false)), 'the running row before the peer').toEqual([
      'text',
      'report',
      'leaves',
    ]);

    // **The carry replaces THE ROW, and the row is no longer the last unit.**
    // A counter draws nothing, so this frame takes `advance` rather than a
    // whole fold, and an advance that replaced the tail would drop the peer row
    // and leave two running rows on one key.
    turn = { messages: [...turn.messages, counted(40)] };
    expect(turnFold(turn, null, true, false), 'the carried fold agrees with a whole one').toEqual(
      fold(turn.messages, null, true, false),
    );
    expect(kinds(turnFold(turn, null, true, false))).toEqual(['text', 'report', 'leaves']);
  });
});
