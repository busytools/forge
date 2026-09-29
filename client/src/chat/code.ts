/**
 * A source file a call read, as the page colours it.
 *
 * **`highlight.js` rather than a highlighter of our own**, on the project's
 * own rule: reach for the maintained module and keep it maintained there. The
 * Rust renderer this replaces bundled `syntect` and a syntax set of its own;
 * this one registers the languages the session actually reads and leaves the
 * rest as plain text.
 *
 * The colours come from the module's theme, imported beside this file. The
 * client's own token set carries none: `theme.ts` drops the five syntax
 * tokens deliberately, and the comment there names this arrangement - the
 * colouring is the client's, from a module that brings its own theme.
 */

import hljs from 'highlight.js/lib/core';
import bash from 'highlight.js/lib/languages/bash';
import css from 'highlight.js/lib/languages/css';
import go from 'highlight.js/lib/languages/go';
import ini from 'highlight.js/lib/languages/ini';
import javascript from 'highlight.js/lib/languages/javascript';
import json from 'highlight.js/lib/languages/json';
import plaintext from 'highlight.js/lib/languages/plaintext';
import python from 'highlight.js/lib/languages/python';
import rust from 'highlight.js/lib/languages/rust';
import sql from 'highlight.js/lib/languages/sql';
import typescript from 'highlight.js/lib/languages/typescript';
import xml from 'highlight.js/lib/languages/xml';
import yaml from 'highlight.js/lib/languages/yaml';
import 'highlight.js/styles/github-dark.css';

/**
 * The languages this page has, by the name `languageOf` answers.
 *
 * A curated set rather than `highlight.js/lib/common`: the full bundle carries
 * every language it has ever learned, and a session reads nine of them.
 */
const LANGUAGES = {
  bash,
  css,
  go,
  ini,
  javascript,
  json,
  // What a file this page has no language for is drawn as: registering it is
  // what keeps the fallback path escaping rather than throwing.
  plaintext,
  python,
  rust,
  sql,
  typescript,
  xml,
  yaml,
} as const;

for (const [name, language] of Object.entries(LANGUAGES)) hljs.registerLanguage(name, language);

/**
 * The language a path names, or `null` for one this page does not colour.
 *
 * A file with no extension and a file whose extension names no language both
 * answer `null`, which draws the text as it came. That is deliberate: a wrong
 * language colours the wrong tokens, and a reader trusts colour more than they
 * should.
 */
export function languageOf(path: string): string | null {
  const cut = path.lastIndexOf('.');
  if (cut === -1) return null;
  const extension = path.slice(cut + 1).toLowerCase();
  const named: Record<string, keyof typeof LANGUAGES> = {
    rs: 'rust',
    ts: 'typescript',
    tsx: 'typescript',
    js: 'javascript',
    jsx: 'javascript',
    mjs: 'javascript',
    py: 'python',
    go: 'go',
    sh: 'bash',
    bash: 'bash',
    zsh: 'bash',
    // `ini` is what toml is highlighted as; there is no toml grammar here.
    toml: 'ini',
    json: 'json',
    yaml: 'yaml',
    yml: 'yaml',
    css: 'css',
    html: 'xml',
    sql: 'sql',
  };
  const language = named[extension];
  return language === undefined ? null : language;
}

/**
 * `text` as highlighted markup, escaped.
 *
 * A language this page does not have, or text the highlighter cannot parse,
 * comes back escaped rather than raw: the page never invents text a reader
 * cannot use, and it never lets a file through as markup either.
 */
export function renderCode(language: string | null, text: string): string {
  if (language !== null && hljs.getLanguage(language) !== undefined) {
    return hljs.highlight(text, { language, ignoreIllegals: true }).value;
  }
  return hljs.highlight(text, { language: 'plaintext', ignoreIllegals: true }).value;
}
