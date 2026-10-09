/**
 * A call's own output, as the wire narrows it once at this boundary.
 *
 * The core answers a `read_call_output` ask with the tail of the file the
 * call wrote to, or a named reason there is none. A shape this build does
 * not know narrows to `unknown` rather than to nothing: rule 25's default is
 * to draw a frame as itself, and an answer read as nothing is a blank the
 * reader cannot tell from a call that never wrote.
 */

/** One answer to a call-output read. */
export type CallOutput =
  | { kind: 'lines'; lines: string[] }
  | { kind: 'file_gone' }
  | { kind: 'no_path' }
  | { kind: 'unknown' };

/** The answer a `call_output` update carries, narrowed at the wire. */
export function callOutputOf(value: unknown): CallOutput {
  if (value === 'file_gone') return { kind: 'file_gone' };
  if (value === 'no_path') return { kind: 'no_path' };
  if (typeof value === 'object' && value !== null) {
    const lines = (value as { lines?: unknown }).lines;
    if (Array.isArray(lines) && lines.every((line) => typeof line === 'string')) {
      return { kind: 'lines', lines };
    }
  }
  return { kind: 'unknown' };
}
