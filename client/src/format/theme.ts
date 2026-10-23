/**
 * The chat's code theme: shiki's `github-dark` structure in highlight.js's
 * `github-dark` palette.
 *
 * **The two themes are not the same palette.** Shiki's `github-dark` is the
 * older GitHub palette (`#f97583` keywords, `#9ecbff` strings), while the
 * highlight.js stylesheet the fenced blocks use is the newer one
 * (`#ff7b72`, `#a5d6ff`, `#8b949e` comments, `#d2a8ff` entities,
 * `#c9d1d9` text). A Read row and a fence in the same conversation must be
 * identical hex for hex, so the scope set is shiki's (which already matched
 * the old panel's shape) and every colour is the highlight.js stylesheet's.
 *
 * Scopes the shiki theme names differently from a highlight.js class are
 * overridden after the remap: markup headings, list bullets, and the
 * inserted/deleted pairs.
 */

import githubDark from '@shikijs/themes/github-dark';
import type { ThemeRegistrationRaw } from 'shiki';

/** The name the highlighter registers this theme under. */
export const HLJS_THEME = 'hljs-github-dark';

/** Shiki's older hexes mapped to the highlight.js `github-dark` stylesheet's. */
const PALETTE: Readonly<Record<string, string>> = {
  '#6a737d': '#8b949e',
  '#79b8ff': '#79c0ff',
  '#b392f0': '#d2a8ff',
  '#e1e4e8': '#c9d1d9',
  '#85e89d': '#7ee787',
  '#f97583': '#ff7b72',
  '#9ecbff': '#a5d6ff',
  '#ffab70': '#ffa657',
  '#dbedff': '#a5d6ff',
  '#d1d5da': '#c9d1d9',
  '#24292e': '#0d1117',
};

function painted(value: string): string {
  return PALETTE[value.toLowerCase()] ?? value;
}

/**
 * The theme's raw settings.
 *
 * The bundled module is the theme JSON with `tokenColors`; the `settings`
 * field the type names is produced by shiki's loader, so the raw field is
 * what is read here.
 */
const RAW = githubDark as unknown as {
  tokenColors?: {
    scope?: string | string[];
    settings: { foreground?: string; background?: string; fontStyle?: string };
  }[];
};

/** The remapped settings, one entry per shiki theme setting. */
const SETTINGS = (RAW.tokenColors ?? []).map((setting) => {
  const mapped: { foreground?: string; background?: string; fontStyle?: string } = {};
  if (setting.settings.foreground !== undefined) {
    mapped.foreground = painted(setting.settings.foreground);
  }
  if (setting.settings.background !== undefined) {
    mapped.background = painted(setting.settings.background);
  }
  if (setting.settings.fontStyle !== undefined) mapped.fontStyle = setting.settings.fontStyle;
  const entry: {
    scope?: string | string[];
    settings: { foreground?: string; background?: string; fontStyle?: string };
  } = { settings: mapped };
  if (setting.scope !== undefined) entry.scope = setting.scope;
  return entry;
});

export const HLJS_GITHUB_DARK: ThemeRegistrationRaw = {
  name: HLJS_THEME,
  type: 'dark',
  colors: { 'editor.foreground': '#c9d1d9', 'editor.background': '#0d1117' },
  settings: [
    ...SETTINGS,
    // The highlight.js stylesheet's own classes where its scope differs from
    // shiki's: a section is `#1f6feb` bold, a bullet `#f2cc60`, and an
    // inserted/deleted pair is GitHub's diff pair.
    {
      scope: ['markup.heading', 'markup.heading entity.name'],
      settings: { foreground: '#1f6feb', fontStyle: 'bold' },
    },
    {
      scope: 'punctuation.definition.list.begin.markdown',
      settings: { foreground: '#f2cc60' },
    },
    {
      scope: ['markup.inserted', 'meta.diff.header.to-file', 'punctuation.definition.inserted'],
      settings: { foreground: '#aff5b4', background: '#033a16' },
    },
    {
      scope: ['markup.deleted', 'meta.diff.header.from-file', 'punctuation.definition.deleted'],
      settings: { foreground: '#ffdcd7', background: '#67060c' },
    },
  ],
};
