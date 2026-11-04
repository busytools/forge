/**
 * The terminal surface: the raised terminal the person opens, and every state
 * between it and the chat they go back to.
 *
 * **One surface at a time, and the terminal outlives it.** Back to chat is
 * not closing the terminal: the shell keeps running, and the next raise
 * attaches again and repaints.
 *
 * A superseded open never lands: the generation is read after every await, so
 * a second open (or a close) while the first is still starting leaves the
 * store on the LATEST request, not the slowest one.
 */

import { SvelteSet } from 'svelte/reactivity';

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import {
  clearTerminalLink,
  installTerminal,
  terminalLink,
  type TerminalOutput,
  type TerminalViewMessage,
} from './link';

export type TerminalPhase = 'closed' | 'opening' | 'live' | 'failed';

interface TerminalSurface {
  phase: TerminalPhase;
  /** The seat whose terminal this is. */
  seat: SessionSlot | null;
  /** The terminal id the server attached. */
  id: number | null;
  /** The server's sentence when the surface could not open. */
  reason: string | null;
  cols: number;
  rows: number;
  /** The last sequence the surface's screen reflects. */
  seq: number;
}

export const surface: TerminalSurface = $state({
  phase: 'closed',
  seat: null,
  id: null,
  reason: null,
  cols: 80,
  rows: 24,
  seq: 0,
});

/** Whether two slots name the same seat. */
export function sameSeat(one: SessionSlot, other: SessionSlot): boolean {
  return one.org === other.org && one.project === other.project && one.label === other.label;
}

/** The newest repaint, for a component that mounts after it landed. */
let newestScreen: string | null = null;
/** The listeners the mounted surface draws through. */
const views = new SvelteSet<(view: TerminalViewMessage) => void>();
const frames = new SvelteSet<(frame: TerminalOutput) => void>();
/**
 * Frames that arrived while no component was mounted to draw them, and
 * whether more arrived than a mount can catch up on.
 *
 * The surface component is a lazy chunk, so a first raise has a window where
 * frames land with nowhere to go; dropping them leaves a hole the repaint
 * predates.
 */
let pending: TerminalOutput[] = [];
let pendingBytes = 0;
let needResync = false;
const PENDING_LIMIT = 1_000_000;

/** Give a mounted surface the newest repaint, then every later one. */
export function onScreen(listener: (view: TerminalViewMessage) => void): () => void {
  views.add(listener);
  if (newestScreen !== null && surface.seat !== null && surface.id !== null) {
    const screen = newestScreen;
    listener({
      seat: surface.seat,
      id: surface.id,
      cols: surface.cols,
      rows: surface.rows,
      seq: surface.seq,
      screen,
      viewers: 1,
    });
  }
  return () => {
    views.delete(listener);
  };
}

/** Every output frame, from now, and the ones held for this mount. */
export function onOutput(listener: (frame: TerminalOutput) => void): () => void {
  frames.add(listener);
  if (needResync) {
    // **A window that outgrew the buffer needs a repaint, not the tail that
    // followed the drop**: the repaint carries the screen and the bytes, and
    // the held tail would draw a fragment of it first.
    needResync = false;
    pending = [];
    pendingBytes = 0;
    void reattach();
  } else if (pending.length > 0) {
    const held = pending;
    pending = [];
    pendingBytes = 0;
    for (const frame of held) {
      // A repaint that landed while these waited already carries them.
      if (frame.seq <= surface.seq) continue;
      surface.seq = frame.seq;
      try {
        listener(frame);
      } catch (why) {
        console.warn('a terminal output listener threw', why);
      }
    }
  }
  return () => {
    frames.delete(listener);
  };
}

/**
 * The terminal hand-off the composer is drawing, so the raised surface's bar
 * can carry its Done and Not now: the dock is behind the surface while it is
 * up, and a parked ask with no reachable answer is a session held for ever.
 */
export const handOff = $state<{
  current: {
    id: string;
    reason: string;
    /** The PTY has ECHO off: what the person types is hidden. */
    secret: boolean;
    respond: (ending: 'done' | 'not_now') => void;
  } | null;
}>({ current: null });

/** Install the link and its pushes for the connection the page draws through. */
export function installTerminalSurface(connection: Connection): () => void {
  installTerminal(connection);
  const link = terminalLink();
  const stopView = link.onView((view) => {
    if (surface.phase === 'closed') return;
    if (surface.seat !== null && !sameSeat(surface.seat, view.seat)) return;
    if (surface.id !== null && view.id !== surface.id) return;
    // **The attach's own answer names the terminal.** A first open names
    // none in its request, so this is where the id first lands - before the
    // open's promise resolves, when the two messages cross.
    surface.id = view.id;
    surface.cols = view.cols;
    surface.rows = view.rows;
    surface.seq = view.seq;
    newestScreen = view.screen;
    for (const listener of views) {
      try {
        listener(view);
      } catch (why) {
        console.warn('a terminal screen listener threw', why);
      }
    }
  });
  const stopFrame = link.onFrame((frame) => {
    // An open's own chunks can land while its attach is still in flight.
    if (surface.phase === 'closed' || frame.id !== surface.id) return;
    // **A chunk the repaint already carries is not written again**: the
    // attach's `seq` is the last one its screen reflects.
    if (frame.seq <= surface.seq) return;
    if (frames.size === 0) {
      // **A frame that lands before the surface mounts is not dropped.** The
      // component is a lazy chunk; hold what arrives until it draws. The
      // sequence does not move until it is drawn, so a repaint that lands in
      // the meantime still supersedes the held copy.
      pending.push(frame);
      pendingBytes += frame.bytes.length;
      if (pendingBytes > PENDING_LIMIT) {
        pending = [];
        pendingBytes = 0;
        needResync = true;
      }
      return;
    }
    surface.seq = frame.seq;
    for (const listener of frames) {
      try {
        listener(frame);
      } catch (why) {
        console.warn('a terminal output listener threw', why);
      }
    }
  });
  // **A reconnect is a new connection, and the attach died with the old one.**
  // A live surface re-attaches and is repainted rather than left drawing a
  // frozen screen under a bar that still says live.
  const stopStatus = connection.onStatus((status) => {
    if (status !== 'open') return;
    if (surface.phase !== 'live' || surface.seat === null || surface.id === null) return;
    void reattach();
  });
  return () => {
    stopView();
    stopFrame();
    stopStatus();
    clearTerminalLink();
  };
}

/** Attach the live surface again after a reconnect, keeping its screen until
 * the repaint lands. */
async function reattach(): Promise<void> {
  const seat = surface.seat;
  const id = surface.id;
  if (seat === null || id === null) return;
  const mine = ++generation;
  try {
    const attached = await terminalLink().show(seat, id);
    if (mine !== generation) return;
    surface.id = attached;
    surface.phase = 'live';
    surface.reason = null;
  } catch (why) {
    if (mine !== generation) return;
    surface.phase = 'failed';
    surface.reason = why instanceof Error ? why.message : String(why);
  }
}

/** The request that is current; a result from an older one is dropped. */
let generation = 0;

/**
 * Raise a seat's terminal.
 *
 * `id` is the terminal the seat's record named, or `null` for a seat that has
 * none: the server opens one, so the person has a door on a seat no agent has
 * touched. Answers why it could not open, or `null` when it did.
 */
export async function openTerminal(seat: SessionSlot, id: number | null): Promise<string | null> {
  const link = terminalLink();
  if (!link.attachable) return 'this page is not connected to a forge';
  const mine = ++generation;
  surface.phase = 'opening';
  surface.seat = seat;
  surface.id = id;
  surface.reason = null;
  newestScreen = null;
  pending = [];
  pendingBytes = 0;
  needResync = false;
  try {
    const attached = await link.show(seat, id);
    if (mine !== generation) return null;
    surface.id = attached;
    surface.phase = 'live';
    return null;
  } catch (why) {
    if (mine !== generation) return null;
    surface.phase = 'failed';
    surface.reason = why instanceof Error ? why.message : String(why);
    return surface.reason;
  }
}

/** Lower the surface; the terminal keeps running. */
export async function closeTerminal(): Promise<string | null> {
  const mine = ++generation;
  const link = terminalLink();
  try {
    await link.hide();
  } catch (why) {
    if (mine !== generation) return null;
    return why instanceof Error ? why.message : String(why);
  }
  if (mine !== generation) return null;
  surface.phase = 'closed';
  surface.seat = null;
  surface.id = null;
  surface.reason = null;
  newestScreen = null;
  pending = [];
  pendingBytes = 0;
  needResync = false;
  return null;
}

/**
 * Tell the server the size the client fitted to, and take a repaint laid out
 * for it.
 *
 * **The attach's repaint is laid out at the server's grid**, which a first
 * open has not yet learned: drawing a 120x30 screen into an 80x24 client
 * wraps it. Answers whether it asked - false when there is nothing to send
 * or the sizes already agree.
 */
export function reconcileSurfaceSize(cols: number, rows: number): boolean {
  const seat = surface.seat;
  const id = surface.id;
  if (seat === null || id === null) return false;
  if (surface.cols === cols && surface.rows === rows) return false;
  terminalLink().resize(seat, id, cols, rows);
  // Record the size asked for, so the next view compares against the grid
  // the client actually has rather than the one this repaint was made for.
  surface.cols = cols;
  surface.rows = rows;
  void reattach();
  return true;
}

/**
 * **A raised surface belongs to the seat it was opened on.** A page that
 * switches seats with one up would draw the next seat's record over the old
 * screen, and its close would end the old seat's shell; lowering it keeps
 * the old terminal running and the new seat's page honest.
 */
export async function lowerForOtherSeat(seat: SessionSlot): Promise<void> {
  const at = surface.seat;
  if (surface.phase === 'closed' || at === null) return;
  if (sameSeat(at, seat)) return;
  await closeTerminal();
}

/** Whether a terminal surface is up. */
export function terminalIsUp(): boolean {
  return surface.phase !== 'closed';
}
