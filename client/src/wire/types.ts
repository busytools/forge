/**
 * The server's shapes, as `crates/forge-server/src/transport/` writes them.
 *
 * These are the payload types rather than the envelope: `ClientMessage`,
 * `ServerMessage` and `Subject` are the socket's own vocabulary and belong
 * with the socket.
 */

import { DEFAULT_AXES, axesFrom, type DictateAxes } from '../session/wire';

/** The seat a session is addressed by. */
export interface SessionSlot {
  org: string;
  project: string;
  label: string;
}

/**
 * The mark, palette and typefaces the server's config picked, from the
 * greeting. `null` on any of them is the built-in rather than a pinned
 * value, because a commented line in a hand-authored `forge.toml` has to
 * mean "unset".
 */
export interface ClientSettings {
  mark: string | null;
  theme: string | null;
  font: string | null;
  /**
   * The axes a client that captures starts on and resets to: `[dictate]`'s
   * keys over the crate's own defaults, which is the same value the panel
   * draws as its unset state.
   */
  dictate: DictateAxes;
}

/** Every key unset, which is what a `forge.toml` with no `[client]` block sends. */
export const DEFAULT_SETTINGS: ClientSettings = {
  mark: null,
  theme: null,
  font: null,
  dictate: DEFAULT_AXES,
};

/** The greeting's settings, narrowed once where they enter. */
export function settingsFrom(value: unknown): ClientSettings {
  const held =
    value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
  return {
    mark: typeof held['mark'] === 'string' ? held['mark'] : null,
    theme: typeof held['theme'] === 'string' ? held['theme'] : null,
    font: typeof held['font'] === 'string' ? held['font'] : null,
    dictate: axesFrom(held['dictate']),
  };
}

/** The marks `[client] mark` accepts, from `forge_primitives::client::MARK_NAMES`. */
export const MARK_NAMES = [
  'panes',
  'klin',
  'lanes',
  'f_slab',
  'split',
  'spine',
  'grid',
  'clamp',
  'strike',
  'nest',
  'chamfer',
  'tally',
  'stencil_f',
] as const;

export type MarkName = (typeof MARK_NAMES)[number];

/** The mark drawn when no name is set. */
export const DEFAULT_MARK: MarkName = 'panes';

/** The palettes `[client] theme` accepts, from `forge_primitives::client::THEME_NAMES`. */
export const THEME_NAMES = ['dark'] as const;

export type ThemeName = (typeof THEME_NAMES)[number];

export const DEFAULT_THEME: ThemeName = 'dark';

/** The typeface sets `[client] font` accepts, from `forge_primitives::client::FONT_NAMES`. */
export const FONT_NAMES = ['system'] as const;

export type FontName = (typeof FONT_NAMES)[number];

/** The port the server binds when `[server] port` is absent. */
export const DEFAULT_SERVER_PORT = 8790;

/**
 * One of `known`, or `fallback` when the value is one this client is older
 * than.
 *
 * The two casts are the boundary's whole job: `known` is read as the strings
 * it holds and the answer is one of them by construction, which is what lets
 * every union downstream stay closed. `Array.includes` cannot narrow, so
 * without them the callers would each need a cast of their own.
 */
export function narrow<T extends string>(value: string, known: T[], fallback: T): T {
  return (known as string[]).includes(value) ? (value as T) : fallback;
}

/** Why dictation's preflight stopped, carrying its own reason. */
export type DictateFailure =
  | { hash_mismatch: { path: string; expected: string; actual: string; size: number } }
  | { cancelled: { kept: number; total: number } }
  | { other: { message: string } };

/** How far one dictation model has got. */
export type DictateModelState =
  | 'pending'
  | 'verifying'
  | 'fetched'
  | 'loading'
  | 'ready'
  | { downloading: { downloaded: number; total: number; resumed_from: number | null } }
  | { failed: DictateFailure };

/** The states that cross as a bare string; the other two are objects. */
const MODEL_STATES: Extract<DictateModelState, string>[] = [
  'pending',
  'verifying',
  'fetched',
  'loading',
  'ready',
];

/**
 * A model's state, narrowed once for both reads that carry it: the home's
 * dictate card and the models page's pinned rows.
 *
 * A state outside the five is one this client is older than, and `pending` is
 * the least-alarming answer - a row that says so claims no load the machine
 * may not be doing.
 */
export function modelStateFrom(state: DictateModelState): DictateModelState {
  return typeof state !== 'string' ? state : narrow(state, MODEL_STATES, 'pending');
}
