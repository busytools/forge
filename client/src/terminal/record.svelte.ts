/**
 * The seat's terminal as its session record carried it.
 *
 * **One place the strip and the surface both read**, so a page whose record
 * has not re-read still draws the terminal it was told about, and a page
 * whose record carried none draws no row at all.
 */

import type { TerminalSummary } from '../wire/terminal';
import type { SessionSlot } from '../wire/types';

export const terminalRecord = $state<{
  seat: SessionSlot | null;
  terminal: TerminalSummary | null;
}>({ seat: null, terminal: null });

/** Apply a session record's terminal half. */
export function setTerminalRecord(seat: SessionSlot, terminal: TerminalSummary | null): void {
  terminalRecord.seat = seat;
  terminalRecord.terminal = terminal;
}
