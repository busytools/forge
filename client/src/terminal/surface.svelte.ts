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

/** The newest repaint, for a component that mounts after it landed. */
let newestScreen: string | null = null;
/** The listeners the mounted surface draws through. */
const views = new SvelteSet<(view: TerminalViewMessage) => void>();
const frames = new SvelteSet<(frame: TerminalOutput) => void>();

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

/** Every output frame, from now. */
export function onOutput(listener: (frame: TerminalOutput) => void): () => void {
  frames.add(listener);
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
    respond: (ending: 'done' | 'not_now') => void;
  } | null;
}>({ current: null });

/** Install the link and its pushes for the connection the page draws through. */
export function installTerminalSurface(connection: Connection): () => void {
  installTerminal(connection);
  const link = terminalLink();
  const stopView = link.onView((view) => {
    if (surface.id !== null && view.id !== surface.id) return;
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
    if (surface.phase !== 'live' || frame.id !== surface.id) return;
    // **A chunk the repaint already carries is not written again**: the
    // attach's `seq` is the last one its screen reflects.
    if (frame.seq <= surface.seq) return;
    surface.seq = frame.seq;
    for (const listener of frames) {
      try {
        listener(frame);
      } catch (why) {
        console.warn('a terminal output listener threw', why);
      }
    }
  });
  return () => {
    stopView();
    stopFrame();
    clearTerminalLink();
  };
}

/** The request that is current; a result from an older one is dropped. */
let generation = 0;

/**
 * Raise a seat's terminal.
 *
 * `id` is the terminal the seat's record named. Answers why it could not
 * open, or `null` when it did.
 */
export async function openTerminal(seat: SessionSlot, id: number): Promise<string | null> {
  const link = terminalLink();
  if (!link.attachable) return 'this page is not connected to a forge';
  const mine = ++generation;
  surface.phase = 'opening';
  surface.seat = seat;
  surface.id = id;
  surface.reason = null;
  newestScreen = null;
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
  return null;
}

/** Whether a terminal surface is up. */
export function terminalIsUp(): boolean {
  return surface.phase !== 'closed';
}
