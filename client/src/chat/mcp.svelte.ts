/**
 * The session's MCP servers, as the seat's strip row draws them.
 *
 * Held as the record is: one seat is on screen, so one list, replaced whole
 * when the record moves. A server's state is part of the row, so a list that
 * outlived its record would state a settled status the session has left.
 */
import type { McpRow } from '../session/view';

export class Mcp {
  #rows = $state<McpRow[]>([]);

  /** Take the record's rows whole: what the wire says is what a row reads. */
  sync(rows: readonly McpRow[] | null): void {
    this.#rows = [...(rows ?? [])];
  }

  /** How many rows there are: servers, or a read that failed. */
  count(): number {
    return this.#rows.length;
  }

  /** Whether there is anything to draw at all. */
  anything(): boolean {
    return this.#rows.length > 0;
  }

  rows(): readonly McpRow[] {
    return this.#rows;
  }
}

/** The one session's rows, as its record last stated them. */
export const mcp = new Mcp();
