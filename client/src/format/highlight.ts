/**
 * The one shiki highlighter, and the lazy grammar set.
 *
 * **Every grammar is a chunk of its own**, taken from shiki's own lazy map
 * (`shiki/langs`), so a surface loads exactly the grammars it opens and the
 * full set is still available. Dynamically imported by the call panel and the
 * Files reader rather than carried in the main bundle.
 *
 * **The engine is Oniguruma, not shiki's JavaScript regex engine.** The JS
 * engine backtracks catastrophically on some text a diff carries: one
 * 833-character TypeScript hunk took 846 ms in the WKWebView rig and held the
 * frame, against 40 ms here for the same hunk, with the same text in the same
 * colours. The WASM is fetched on the first highlight, which the lazy import
 * already covers.
 */

import { createHighlighterCore, type HighlighterCore } from 'shiki/core';
import { createOnigurumaEngine } from 'shiki/engine/oniguruma';
import type { LanguageRegistration, ThemedToken } from 'shiki';
import { bundledLanguages } from 'shiki/langs';
import githubDark from '@shikijs/themes/github-dark';

import { HLJS_GITHUB_DARK, HLJS_THEME } from './theme';

let shared: Promise<HighlighterCore> | null = null;

/** The in-flight load per grammar, so two rows asking for one grammar share it. */
const loading = new Map<string, Promise<void>>();

function base(): Promise<HighlighterCore> {
  shared ??= createHighlighterCore({
    // Both palettes: the chat's matches the highlight.js fences, and the
    // Files reader keeps the shiki theme it has drawn with.
    themes: [githubDark, HLJS_GITHUB_DARK],
    langs: [],
    engine: createOnigurumaEngine(import('shiki/wasm')),
  });
  return shared;
}

/**
 * A grammar that matches nothing, registered under shiki's special `text`
 * name.
 *
 * **`@cmshiki/shiki` asks `getLanguage` rather than tokenising**, and shiki
 * treats `text` as a special language only inside its own tokenisers - so a
 * reader handed `text` throws unless a registration exists. This is that
 * registration: every character comes back unstyled, which is what plain
 * text is.
 */
const PLAIN: LanguageRegistration = {
  name: 'text',
  scopeName: 'text.plain',
  patterns: [],
  repository: {},
};

/** The highlighter with `language` loaded, `text` included as a no-op grammar. */
export async function highlighterFor(language: string): Promise<HighlighterCore> {
  const highlighter = await base();
  if (language === 'text' || language === 'plaintext') {
    if (!highlighter.getLoadedLanguages().includes('text')) {
      await highlighter.loadLanguage(PLAIN);
    }
    return highlighter;
  }
  if (highlighter.getLoadedLanguages().includes(language)) return highlighter;
  const load = (bundledLanguages as Record<string, (() => Promise<unknown>) | undefined>)[language];
  if (load === undefined) return highlighter;
  let pending = loading.get(language);
  if (pending === undefined) {
    pending = load()
      .then(async (module) => {
        await highlighter.loadLanguage((module as { default: never }).default);
      })
      // A rejected promise left in the map would fail every later open of
      // that language forever; a retry is owed the next chunk fetch.
      .catch((why: unknown) => {
        loading.delete(language);
        throw why;
      });
    loading.set(language, pending);
  }
  await pending;
  return highlighter;
}

/** `text` escaped, so a token's own characters never become markup. */
function escaped(text: string): string {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/** One token as its span: the theme's colour, and its slant where it has one. */
function tokenSpan(token: ThemedToken): string {
  const bits = Number(token.fontStyle ?? 0);
  const styles = [
    token.color === undefined ? '' : `color:${token.color}`,
    bits === 0 ? '' : tokenFontStyle(bits),
  ].filter((style) => style !== '');
  const style = styles.length === 0 ? '' : ` style="${styles.join(';')}"`;
  return `<span${style}>${escaped(token.content)}</span>`;
}

/** shiki's font-style bits: italic, bold, underline. */
function tokenFontStyle(bits: number): string {
  const styles = [
    (bits & 1) === 1 ? 'font-style:italic' : '',
    (bits & 2) === 2 ? 'font-weight:bold' : '',
    (bits & 4) === 4 ? 'text-decoration:underline' : '',
  ].filter((style) => style !== '');
  return styles.join(';');
}

/**
 * `text` as the coloured body of a code panel, one span run per line.
 *
 * The chat's theme is the highlight.js palette, so a Read panel and a fenced
 * block are the same colours hex for hex.
 */
export async function highlightedBody(language: string, text: string): Promise<string> {
  const highlighter = await highlighterFor(language);
  const lines = highlighter.codeToTokensBase(text, { lang: language, theme: HLJS_THEME });
  return lines.map((line) => line.map(tokenSpan).join('')).join('\n');
}

/** The same tokenisation, line by line, for a surface that decorates itself. */
export async function tokenisedLines(language: string, text: string): Promise<ThemedToken[][]> {
  const highlighter = await highlighterFor(language);
  return highlighter.codeToTokensBase(text, { lang: language, theme: HLJS_THEME });
}
