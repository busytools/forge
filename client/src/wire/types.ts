/**
 * The server's shapes, as `crates/forge-server/src/transport/` writes them.
 *
 * These are the payload types rather than the envelope: `ClientMessage`,
 * `ServerMessage` and `Subject` are the socket's own vocabulary and belong
 * with the socket.
 */

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
}

/** Every key unset, which is what a `forge.toml` with no `[web]` block sends. */
export const DEFAULT_SETTINGS: ClientSettings = { mark: null, theme: null, font: null };

/** The marks `[web] mark` accepts, from `forge_primitives::web::MARK_NAMES`. */
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

/** The palettes `[web] theme` accepts, from `forge_primitives::web::THEME_NAMES`. */
export const THEME_NAMES = ['dark'] as const;

export type ThemeName = (typeof THEME_NAMES)[number];

export const DEFAULT_THEME: ThemeName = 'dark';

/** The typeface sets `[web] font` accepts, from `forge_primitives::web::FONT_NAMES`. */
export const FONT_NAMES = ['system'] as const;

export type FontName = (typeof FONT_NAMES)[number];

/** The port the server binds when `[web] port` is absent. */
export const DEFAULT_WEB_PORT = 8790;
