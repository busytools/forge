/**
 * The palettes and typefaces forge ships, by the names `[web] theme` and
 * `[web] font` take - the client half of `crates/forge-web/src/theme.rs`.
 *
 * The values arrive from the server in the greeting rather than from a file
 * here: `forge.toml` is the server's, one source means one reader, and a
 * second reader is how the two drift.
 *
 * Every surface reads these tokens and nothing else, so a theme is one place
 * to change and no component branches for it.
 *
 * The greeting carries the NAMES - `dark`, `system`, `klin` - and this file
 * holds the values they resolve to. A name outside the shipped set resolves
 * to nothing, which is the server's boot refusal rather than a client
 * fallback.
 */

import type { ClientSettings, FontName, ThemeName } from './wire/types';

/** The built-in pair, drawn with the faces shipped in `public/fonts/`. */
const BUILT_IN_FONT = {
  ui: '"Inter",system-ui,-apple-system,"Segoe UI",sans-serif',
  mono: '"Fira Code",ui-monospace,Menlo,monospace',
};

/** The stacks the OS already has, which is what `system` asks for. */
const SYSTEM_FONT = {
  ui: 'system-ui,-apple-system,"Segoe UI",sans-serif',
  mono: 'ui-monospace,Menlo,monospace',
};

/**
 * Every token the client's own surfaces read.
 *
 * `web.css` declares none of these - only its own layout and type scale -
 * so the palette has exactly one home and this is it.
 *
 * The server's own `theme.rs` carries five more, `--syn-key`, `--syn-str`,
 * `--syn-fn`, `--add-bg` and `--del-bg`, and they are deliberately NOT here:
 * they colour code and a diff, which the server does not own. Colouring is
 * the client's, from a maintained highlighter module that brings its own
 * theme, and `web.css`'s `.k` / `.s` / `.f` / `.dif` rules go with it when
 * the chat's code rendering lands.
 */
const DARK: Record<string, string> = {
  '--bg': '#04050a',
  '--s1': '#0d111a',
  '--s2': '#141926',
  '--s3': '#1b2130',
  '--line': '#222a3c',
  '--text': '#eaeef6',
  '--muted': '#8f98a8',
  '--dim': '#5d6675',
  '--accent': '#f47600',
  '--ok': '#82c76b',
  '--warn': '#c9a13b',
  '--bad': '#e0683e',
  '--blue': '#61a0e0',
  '--teal': '#4ec9c9',
  '--violet': '#b79ae0',
  '--hot': '#ffb058',
};

/**
 * The tokens a palette name resolves.
 *
 * Dark is the only palette shipped, so every name draws it - including one
 * outside `THEME_NAMES`, which the server refuses at boot. A second palette
 * joins here.
 */
export function rootTokens(_theme: ThemeName | null | string): Record<string, string> {
  return DARK;
}

/**
 * The font stack a name resolves, or `null` for a name outside the shipped
 * set.
 *
 * A name nobody ships draws no stack rather than the built-in one: the
 * server refuses such a config at boot, and dressing it as the built-in pair
 * would make an ignored name read as the key working.
 */
export function fontStack(name: FontName | null | string): { ui: string; mono: string } | null {
  if (name === null) return BUILT_IN_FONT;
  if (name === 'system') return SYSTEM_FONT;
  return null;
}

/** The one thing a root has to offer: somewhere to hang a custom property. */
export interface StyleTarget {
  style: {
    setProperty(token: string, value: string): void;
    removeProperty(token: string): void;
  };
}

/**
 * Apply what the greeting carried, to the document root.
 *
 * Inline custom properties rather than a `<style>` block: an injected rule
 * ties with the sheet's own `:root` on specificity and loses on document
 * order, which is how a font stack silently does nothing.
 */
export function applySettings(settings: ClientSettings, root: StyleTarget): void {
  for (const [token, value] of Object.entries(rootTokens(settings.theme))) {
    root.style.setProperty(token, value);
  }

  const stack = fontStack(settings.font);
  if (stack) {
    root.style.setProperty('--ui', stack.ui);
    root.style.setProperty('--mono', stack.mono);
    return;
  }
  // A name outside the shipped set contributes nothing, so a stack applied
  // by an earlier greeting must go rather than linger.
  root.style.removeProperty('--ui');
  root.style.removeProperty('--mono');
}
