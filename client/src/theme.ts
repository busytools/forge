/**
 * The palettes and typefaces forge ships, by the names `[web] theme` and
 * `[web] font` take - the client half of `crates/forge-web/src/theme.rs`.
 *
 * The greeting carries the NAMES - `dark`, `system`, `klin` - and this file
 * holds the values they resolve to. So the client reads no config file: one
 * source means one reader, and what crosses is which of forge's shipped sets
 * to draw with rather than the set itself. A name outside the shipped list
 * resolves to nothing, which is the server's boot refusal rather than a
 * client fallback.
 *
 * Every surface reads these tokens and nothing else, so a theme is one place
 * to change and no component branches for it.
 */

import type { ClientSettings } from './wire/types';

/** The built-in face, drawn with the faces shipped in `public/fonts/`. */
const BUILT_IN_FONT = {
  ui: '"Fira Code",system-ui,-apple-system,"Segoe UI",sans-serif',
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
  // The selected option row's ground. Flat rather than the accent at 10%
  // over the dock's gradient, so --dim on it is one number (4.65:1) and the
  // contrast check can carry the pair.
  '--sel': '#20140a',
  '--line': '#222a3c',
  // The mark on a control with no fill or shadow to say so, which 1.4.11
  // asks 3:1 of: 3.50:1 on the page, 3.23-3.36 on the dock's gradient where
  // the option checkbox sits. --line stays the hairline.
  '--ctl': '#516590',
  // Prose, and the page's brightest text: a grey rather than a near-white, and
  // the value the highlighter's github-dark theme names for code text rather
  // than one picked to taste. Its base rule needs a `.hljs` container the code
  // block never emits, so this token is still what a block's body text reads.
  // The terminal draws the same text at 6.10:1 (Ghostty `GitHub Dark`), the
  // page cannot follow it that far and stay above --muted, and this lands at
  // 13.19:1, with both ends of its band held in contrast.test.ts.
  '--text': '#c9d1d9',
  '--muted': '#8f98a8',
  // The quietest of the three text tokens, and the darkest step that clears
  // AA on every ground the sheet puts text on: 5.26:1 on the page, 4.87:1 on
  // --s1 and 4.53:1 on --s2. It was #5d6675, which measured 3.51:1 while
  // carrying 12.5px labels in about fifty places, so the token failed the
  // standard rather than the uses being wrong. Kept in step with
  // `crates/forge-web/src/theme.rs` by `salvage.test.ts`.
  '--dim': '#788294',
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
export function rootTokens(_theme: string | null): Record<string, string> {
  return DARK;
}

/**
 * The font stack a name resolves, or `null` for a name outside the shipped
 * set.
 *
 * A name nobody ships draws no stack rather than the built-in one: the
 * server refuses such a config at boot, and dressing it as the built-in face
 * would make an ignored name read as the key working.
 */
export function fontStack(name: string | null): { ui: string; mono: string } | null {
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
