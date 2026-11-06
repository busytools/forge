/**
 * The terminal's wire shapes, narrowed once where they enter.
 *
 * The binary stream is its own frame kinds - [`TERMINAL_OUTPUT_TAG`] and
 * [`TERMINAL_INPUT_TAG`], from `forge_primitives::terminal` - and the JSON
 * here is the strip's row and the state a surface draws. A value the server
 * sent that this build cannot name is kept as `unknown` rather than dropped,
 * so a newer server's field draws plainly instead of vanishing.
 */

/** The binary kind a terminal's output crosses as. */
export const TERMINAL_OUTPUT_TAG = 4;

/** The binary kind a client's terminal input crosses as. */
export const TERMINAL_INPUT_TAG = 5;

/** What kind of input the terminal is waiting for. */
export type TerminalInputKind = 'password' | 'confirm' | 'prompt';

/** One input the terminal is waiting for, with the evidence behind it. */
export interface TerminalInputExpected {
  kind: TerminalInputKind;
  evidence: string;
}

/** A command's exit. */
export interface TerminalExit {
  code: number | null;
  signal: number | null;
}

/** Where a state answer came from. */
export type TerminalSource = 'exact' | 'heuristic';

/** What the terminal is doing, as the agent reads it. */
export interface TerminalState {
  running: boolean;
  input_expected: TerminalInputExpected | null;
  exit: TerminalExit | null;
  source: TerminalSource;
}

/** One terminal as the strip draws it. */
export interface TerminalSummary {
  id: number;
  shell: string;
  cwd: string;
  cols: number;
  rows: number;
  alive: boolean;
  state: TerminalState;
  title: string;
  held: boolean;
}

/** One output frame: the terminal's id, its sequence and the shell's bytes. */
export interface TerminalOutputFrame {
  id: number;
  seq: number;
  bytes: Uint8Array;
}

function held(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function text(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isInteger(value) ? value : 0;
}

function finiteOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** One terminal summary, or `null` when the seat has no terminal. */
export function terminalSummaryFrom(value: unknown): TerminalSummary | null {
  if (value === null || value === undefined) return null;
  const raw = held(value);
  return {
    id: count(raw['id']),
    shell: text(raw['shell']),
    cwd: text(raw['cwd']),
    cols: count(raw['cols']),
    rows: count(raw['rows']),
    alive: raw['alive'] === true,
    state: terminalStateFrom(raw['state']),
    title: text(raw['title']),
    held: raw['held'] === true,
  };
}

/** One state, with every unknown narrowed to the safe side. */
export function terminalStateFrom(value: unknown): TerminalState {
  const raw = held(value);
  const expected = raw['input_expected'];
  const exit = raw['exit'];
  const kind = held(expected)['kind'];
  return {
    running: raw['running'] === true,
    input_expected:
      expected === null || expected === undefined
        ? null
        : {
            kind: kind === 'password' || kind === 'confirm' || kind === 'prompt' ? kind : 'prompt',
            evidence: text(held(expected)['evidence']),
          },
    exit:
      exit === null || exit === undefined
        ? null
        : {
            code: finiteOrNull(held(exit)['code']),
            signal: finiteOrNull(held(exit)['signal']),
          },
    source: raw['source'] === 'exact' ? 'exact' : 'heuristic',
  };
}

/**
 * One binary message, decoded as a terminal output frame, when it is one.
 *
 * The tag, a little-endian `u16` id and `u32` sequence, then the raw bytes the
 * shell wrote. A frame shorter than its header is `null`, so the caller
 * reports it rather than drawing bytes it cannot place.
 */
export function terminalOutputFrame(bytes: Uint8Array): TerminalOutputFrame | null {
  if (bytes.length < 7 || bytes[0] !== TERMINAL_OUTPUT_TAG) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return { id: view.getUint16(1, true), seq: view.getUint32(3, true), bytes: bytes.subarray(7) };
}

/** One client input frame's bytes: the tag, the id, then the keystrokes. */
export function terminalInputFrame(id: number, bytes: Uint8Array): Uint8Array {
  const frame = new Uint8Array(3 + bytes.length);
  frame[0] = TERMINAL_INPUT_TAG;
  new DataView(frame.buffer).setUint16(1, id, true);
  frame.set(bytes, 3);
  return frame;
}
