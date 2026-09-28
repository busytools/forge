/**
 * The page's icon set, taken verbatim from `crates/forge-web/src/icons.rs`.
 *
 * Lucide, MIT, is the web-native set, so no character-cell glyph from the
 * TUI's table survives here. The sprite is inlined once per page and every
 * icon is a `<use>` reference to it, which is what lets a page's own rule
 * colour an icon by the row it sits in.
 */

import sprite from '../assets/sprite.svg?raw';

export const SPRITE = sprite;

/** Every id the sprite defines, without its `i-` prefix. */
export const ICONS = [...sprite.matchAll(/<symbol id="i-([a-z0-9-]+)"/g)].map((match) => match[1]);
