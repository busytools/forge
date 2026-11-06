/**
 * The terminal's link: the one seam between what the page draws and the
 * seat's terminal on the server.
 *
 * **Two clients, one interface.** Inside the shell the page does not hold the
 * socket, so every message rides a Tauri command and the output arrives as a
 * `client://terminal-frame` event. A page with no shell holds the socket
 * itself, so the same calls go through the `TerminalWire` half of the
 * connection and the frames arrive on its binary stream.
 *
 * Nothing above this file knows which it is.
 */

import type { InvokeArgs } from '@tauri-apps/api/core';

import type { Connection, TerminalWire } from '../socket';
import type { SessionSlot } from '../wire/types';

/** One output frame, as a surface draws it. */
export interface TerminalOutput {
  id: number;
  seq: number;
  bytes: Uint8Array;
}

/** What the surface store and the dock call. */
export interface TerminalLink {
  /**
   * Attach this connection to a seat's terminal; answers the id it attached.
   * `id` is null when the seat has no terminal yet: the server opens one.
   * **`cols` and `rows` are the grid the client will draw on**, so the
   * repaint comes laid out for it rather than being skipped.
   */
  show(
    seat: SessionSlot,
    id: number | null,
    cols: number | null,
    rows: number | null,
  ): Promise<number>;
  /** Give the surface up; the terminal keeps running. */
  hide(): Promise<void>;
  /** One client input frame: the keystrokes, as bytes. */
  input(id: number, bytes: Uint8Array): void;
  /** The surface changed size, in columns and rows. */
  resize(seat: SessionSlot, id: number, cols: number, rows: number): void;
  /** Stop or resume the byte stream. */
  pause(paused: boolean): void;
  /** Take the person's control, or give it back. */
  control(seat: SessionSlot, id: number, take: boolean): void;
  /** Every output frame, with the id it belongs to. */
  onFrame(listener: (frame: TerminalOutput) => void): () => void;
  /** Every attach snapshot: the screen as a VT stream and where it continues. */
  onView(listener: (view: TerminalViewMessage) => void): () => void;
  /** Every controller change for the attached terminal. */
  onControl(listener: (state: TerminalControlMessage) => void): () => void;
  /** Whether there is a link to call at all. */
  readonly attachable: boolean;
}

/** One attach snapshot, as the server writes it. */
export interface TerminalViewMessage {
  seat: SessionSlot;
  id: number;
  cols: number;
  rows: number;
  seq: number;
  screen: string;
  viewers: number;
  /** This connection's own viewer number. */
  viewer: number;
  /** Who holds the person's control, or null for the agent. */
  controller: number | null;
}

/** Who holds a terminal's input, as the server pushes it on a change. */
export interface TerminalControlMessage {
  seat: SessionSlot;
  id: number;
  controller: number | null;
}

function viewOf(value: unknown): TerminalViewMessage | null {
  if (typeof value !== 'object' || value === null) return null;
  const held = value as Record<string, unknown>;
  if (typeof held['id'] !== 'number' || typeof held['seq'] !== 'number') return null;
  if (typeof held['screen'] !== 'string') return null;
  return {
    seat: held['seat'] as SessionSlot,
    id: held['id'],
    cols: typeof held['cols'] === 'number' ? held['cols'] : 80,
    rows: typeof held['rows'] === 'number' ? held['rows'] : 24,
    seq: held['seq'],
    screen: held['screen'],
    viewers: typeof held['viewers'] === 'number' ? held['viewers'] : 0,
    viewer: typeof held['viewer'] === 'number' ? held['viewer'] : 0,
    controller: typeof held['controller'] === 'number' ? held['controller'] : null,
  };
}

/** One controller change, as the server writes it. */
function controlOf(value: unknown): TerminalControlMessage | null {
  if (typeof value !== 'object' || value === null) return null;
  const held = value as Record<string, unknown>;
  if (typeof held['id'] !== 'number') return null;
  return {
    seat: held['seat'] as SessionSlot,
    id: held['id'],
    controller: typeof held['controller'] === 'number' ? held['controller'] : null,
  };
}

/** The shell's `invoke`, loaded lazily so this module reads outside Tauri. */
async function invokeShell<T>(command: string, args?: InvokeArgs): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke<T>(command, args);
}

/** Whether this page is inside the client's shell. */
function inShell(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

/** The shell's link: Tauri commands out, events in. */
function shellLink(): TerminalLink {
  return {
    attachable: true,
    async show(seat, id, cols, rows) {
      const answer: unknown = await invokeShell('client_terminal_view', { seat, id, cols, rows });
      const held =
        typeof answer === 'object' && answer !== null ? (answer as Record<string, unknown>) : {};
      if (held['ok'] !== true) {
        throw new Error(
          typeof held['why'] === 'string' && held['why'] !== ''
            ? held['why']
            : 'the terminal could not be opened',
        );
      }
      if (typeof held['id'] !== 'number') throw new Error('the attach named no terminal');
      return held['id'];
    },
    async hide() {
      await invokeShell('client_terminal_detach');
    },
    input(id, bytes) {
      void invokeShell('client_terminal_input', { id, bytes: Array.from(bytes) }).catch((why) => {
        console.warn('a terminal input frame could not be sent', why);
      });
    },
    resize(seat, id, cols, rows) {
      void invokeShell('client_terminal_resize', { seat, id, cols, rows }).catch((why) => {
        console.warn('the terminal was not resized', why);
      });
    },
    pause(paused) {
      void invokeShell('client_terminal_pause', { paused }).catch((why) => {
        console.warn('the terminal stream was not paused', why);
      });
    },
    control(seat, id, take) {
      void invokeShell('client_terminal_control', { seat, id, take }).catch((why) => {
        console.warn('the terminal control did not change', why);
      });
    },
    onFrame(listener) {
      let stop: (() => void) | null = null;
      let live = true;
      void (async () => {
        const { listen } = await import('@tauri-apps/api/event');
        const off = await listen<{ id: number; seq: number; bytes: string }>(
          'client://terminal-frame',
          (event) => {
            const { id, seq, bytes } = event.payload;
            // **The bytes stay bytes**: base64 is the shell's JSON carrier,
            // not a new encoding the surface has to parse.
            const raw = atob(bytes);
            const frame = new Uint8Array(raw.length);
            for (let index = 0; index < raw.length; index += 1) {
              frame[index] = raw.charCodeAt(index);
            }
            listener({ id, seq, bytes: frame });
          },
        );
        if (live) stop = off;
        else off();
      })().catch((why) => {
        console.warn('the terminal frame listener could not start', why);
      });
      return () => {
        live = false;
        stop?.();
      };
    },
    onView(listener) {
      let stop: (() => void) | null = null;
      let live = true;
      void (async () => {
        const { listen } = await import('@tauri-apps/api/event');
        const off = await listen<unknown>('terminal_view', (event) => {
          const view = viewOf(event.payload);
          if (view !== null) listener(view);
        });
        if (live) stop = off;
        else off();
      })().catch((why) => {
        console.warn('the terminal view listener could not start', why);
      });
      return () => {
        live = false;
        stop?.();
      };
    },
    onControl(listener) {
      let stop: (() => void) | null = null;
      let live = true;
      void (async () => {
        const { listen } = await import('@tauri-apps/api/event');
        const off = await listen<unknown>('terminal_control', (event) => {
          const state = controlOf(event.payload);
          if (state !== null) listener(state);
        });
        if (live) stop = off;
        else off();
      })().catch((why) => {
        console.warn('the terminal control listener could not start', why);
      });
      return () => {
        live = false;
        stop?.();
      };
    },
  };
}

/** The page's link: the connection's own terminal half. */
function wireLink(connection: Connection & TerminalWire): TerminalLink {
  return {
    attachable: true,
    show: (seat, id, cols, rows) => connection.terminalView(seat, id, cols, rows),
    hide: () => {
      connection.terminalDetach();
      return Promise.resolve();
    },
    input: (id, bytes) => {
      connection.terminalInput(id, bytes);
    },
    resize: (seat, id, cols, rows) => {
      if (!connection.terminalResize(seat, id, cols, rows)) {
        console.warn('the terminal was not resized: the socket is not open');
      }
    },
    pause: (paused) => {
      if (!connection.terminalPause(paused)) {
        console.warn('the terminal stream was not paused: the socket is not open');
      }
    },
    control: (seat, id, take) => {
      if (!connection.terminalControl(seat, id, take)) {
        console.warn('the terminal control did not change: the socket is not open');
      }
    },
    onFrame: (listener) => connection.onTerminalFrame(listener),
    onView: (listener) =>
      connection.onMessage((message) => {
        if (message.kind !== 'terminal_view') return;
        const view = viewOf(message);
        if (view !== null) listener(view);
      }),
    onControl: (listener) =>
      connection.onMessage((message) => {
        if (message.kind !== 'terminal_control') return;
        const state = controlOf(message);
        if (state !== null) listener(state);
      }),
  };
}

let link: TerminalLink | null = null;

/** Install the link for the connection the page draws through. */
export function installTerminal(connection: Connection): void {
  link = inShell() ? shellLink() : wireLink(connection as Connection & TerminalWire);
}

/** Give the link up with the connection it was made for. */
export function clearTerminalLink(): void {
  link = null;
}

/** The current link, or one that refuses by name. */
export function terminalLink(): TerminalLink {
  return (
    link ?? {
      attachable: false,
      show: () => Promise.reject(new Error('this page is not connected to a forge')),
      hide: async () => {},
      input: () => {},
      resize: () => {},
      pause: () => {},
      control: () => {},
      onFrame: () => () => {},
      onView: () => () => {},
      onControl: () => () => {},
    }
  );
}
