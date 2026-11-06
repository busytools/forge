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
import type { TerminalSummary } from '../wire/terminal';

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
  /** This connection's viewer number for the attached terminal. */
  viewer: number | null;
  /** The viewer holding the person's control, or null for the agent. */
  controller: number | null;
  /** A take or release the person sent that the server has not answered yet. */
  control_pending: boolean | null;
  /** The server's sentence when a take or release was refused. */
  control_reason: string | null;
}

export const surface: TerminalSurface = $state({
  phase: 'closed',
  seat: null,
  id: null,
  reason: null,
  cols: 80,
  rows: 24,
  seq: 0,
  viewer: null,
  controller: null,
  control_pending: null,
  control_reason: null,
});

/** Whether two slots name the same seat. */
export function sameSeat(one: SessionSlot, other: SessionSlot): boolean {
  return one.org === other.org && one.project === other.project && one.label === other.label;
}

/** The newest repaint, for a component that mounts after it landed. */
let newestScreen: string | null = null;
/** The open the mounted surface answers once it has a grid to draw on. */
let pendingOpen: { gen: number; settle: (why: string | null) => void } | null = null;
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
      viewer: surface.viewer ?? 0,
      controller: surface.controller,
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
    surface.viewer = view.viewer;
    surface.controller = view.controller;
    // **A repaint is the server's current truth**: any press still waiting
    // or refusal still drawn belongs to the attach that is being replaced.
    surface.control_pending = null;
    surface.control_reason = null;
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
  const stopControl = link.onControl((state) => {
    if (surface.phase === 'closed') return;
    if (surface.seat !== null && !sameSeat(surface.seat, state.seat)) return;
    if (surface.id !== null && state.id !== surface.id) return;
    // **Losing the keyboard is said, not merely drawn.** A client that held
    // the role and finds it gone hears which kind of hand took it: another
    // client's name is on the bar, and an agent's turn says so.
    const lost = surface.viewer !== null && surface.controller === surface.viewer;
    surface.controller = state.controller;
    // The answer landed, whatever it was: the bar draws the role, not a wait.
    surface.control_pending = null;
    surface.control_reason =
      lost && state.controller === null ? 'the agent took the keyboard' : null;
  });
  // **A refused take or release is drawn.** The push is the success path;
  // the server's own refusal names this operation, and a bar that showed
  // neither would leave the press looking like it worked.
  const stopErrors = connection.onMessage((message) => {
    if (message.kind !== 'error' || message.what !== 'terminal_control') return;
    if (surface.phase === 'closed') return;
    surface.control_pending = null;
    surface.control_reason = message.why;
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
    stopControl();
    stopErrors();
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
    // **The repaint is asked for at the client's own grid**, so it draws the
    // moment it lands rather than being skipped for a second round trip.
    const attached = await terminalLink().show(seat, id, surface.cols, surface.rows);
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
  surface.viewer = null;
  surface.controller = null;
  surface.control_pending = null;
  surface.control_reason = null;
  newestScreen = null;
  pending = [];
  pendingBytes = 0;
  needResync = false;
  // **The attach waits for the surface that will draw it.** The grid comes
  // from the mounted terminal's own fit, so the first repaint is laid out
  // for the client instead of being skipped and re-asked for - the flush the
  // person saw when a second client attached.
  pendingOpen?.settle('the terminal was opened again');
  return await new Promise((resolve) => {
    pendingOpen = { gen: mine, settle: resolve };
  });
}

/**
 * Attach the surface once it has a grid to draw on, answering the open the
 * row is waiting for. Called by the mounted surface after its fit.
 */
export async function attachSurface(cols: number, rows: number): Promise<void> {
  const seat = surface.seat;
  if (seat === null) return;
  const mine = generation;
  const id = surface.id;
  try {
    const attached = await terminalLink().show(seat, id, cols, rows);
    if (mine !== generation) return;
    surface.id = attached;
    surface.cols = cols;
    surface.rows = rows;
    surface.phase = 'live';
    surface.reason = null;
    if (pendingOpen?.gen === mine) {
      pendingOpen.settle(null);
      pendingOpen = null;
    }
  } catch (why) {
    if (mine !== generation) return;
    const message = why instanceof Error ? why.message : String(why);
    surface.phase = 'failed';
    surface.reason = message;
    if (pendingOpen?.gen === mine) {
      pendingOpen.settle(message);
      pendingOpen = null;
    }
  }
}

/** Lower the surface; the terminal keeps running. */
export async function closeTerminal(): Promise<string | null> {
  const mine = ++generation;
  // A surface closed before it attached answers the open it was given.
  pendingOpen?.settle(null);
  pendingOpen = null;
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
  surface.viewer = null;
  surface.controller = null;
  surface.control_pending = null;
  surface.control_reason = null;
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

/** Whether this page holds the person's control of the raised terminal. */
export function controlling(): boolean {
  return (
    surface.viewer !== null && surface.controller !== null && surface.controller === surface.viewer
  );
}

/**
 * Whether this client may type into the raised terminal.
 *
 * **Only the controller types, and the controller is always explicit**: a
 * parked hand-off raises the surface as a viewer, and the person takes the
 * keyboard from the bar. Nothing is granted to the first client to type.
 */
export function mayType(): boolean {
  return controlling();
}

/**
 * The bar's state line, from the surface's own facts.
 *
 * **Who holds the keyboard comes first**, whether a hand-off parked it or
 * another client took it through the bar: a client that lost the role reads
 * that, not "view only" while another client types.
 */
export function terminalStateLine(input: {
  phase: TerminalPhase;
  reason: string | null;
  terminal: TerminalSummary | null;
  viewer: number | null;
  controller: number | null;
  controlReason: string | null;
}): string {
  if (input.phase === 'failed') return input.reason ?? 'the terminal stopped answering';
  if (input.phase === 'opening') return 'opening the terminal';
  const held = input.terminal;
  // **Liveness outranks a refusal**: a sentence about a keyboard says
  // nothing about a terminal that is gone.
  if (held === null) return 'the terminal closed';
  if (!held.alive) return 'the shell exited';
  if (input.controlReason !== null) return input.controlReason;
  if (input.viewer !== null && input.controller !== null && input.controller === input.viewer) {
    return 'you have control';
  }
  if (input.controller !== null) return 'another client has the keyboard';
  if (held.held) return 'waiting for you';
  if (held.state.running) return 'the agent is running a command';
  if (
    held.state.input_expected?.kind === 'password' ||
    held.state.input_expected?.kind === 'confirm'
  ) {
    return 'the agent is waiting for input';
  }
  return 'view only';
}

/**
 * Take the person's control of the raised terminal, or give it back. The
 * agent's tools are refused while it is held, exactly as during a hand-off.
 */
export function controlTerminal(take: boolean): void {
  const seat = surface.seat;
  const id = surface.id;
  if (seat === null || id === null) return;
  // **The press is drawn while the server answers**, and a refusal replaces
  // the wait with the server's sentence when it lands.
  surface.control_pending = take;
  surface.control_reason = null;
  terminalLink().control(seat, id, take);
}

/**
 * The hand-off a refusal named has ended: the sentence stops being true.
 *
 * **The ask can resolve without this client pressing anything** - another
 * client answers it, or the seat drops it - so a bar left carrying "a
 * terminal hand-off holds the keyboard" would name an ask that is gone.
 */
export function clearControlRefusal(): void {
  surface.control_reason = null;
}

/** Whether a terminal surface is up. */
export function terminalIsUp(): boolean {
  return surface.phase !== 'closed';
}
