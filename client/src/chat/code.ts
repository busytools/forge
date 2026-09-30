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
 * The languages this page has, by the name an extension or a fence names.
 *
 * One table for both readers: a path's extension and a fence's info string name
 * the same set, and a second table is how the two drift.
 */
const NAMED: Record<string, keyof typeof LANGUAGES> = {
  rs: 'rust',
  rust: 'rust',
  ts: 'typescript',
  tsx: 'typescript',
  typescript: 'typescript',
  js: 'javascript',
  jsx: 'javascript',
  mjs: 'javascript',
  javascript: 'javascript',
  py: 'python',
  python: 'python',
  go: 'go',
  sh: 'bash',
  bash: 'bash',
  zsh: 'bash',
  // `ini` is what toml is highlighted as; there is no toml grammar here.
  toml: 'ini',
  ini: 'ini',
  json: 'json',
  yaml: 'yaml',
  yml: 'yaml',
  css: 'css',
  html: 'xml',
  xml: 'xml',
  sql: 'sql',
};

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
  return namedFor(path.slice(cut + 1));
}

/**
 * The language a fence's info string names, or `null` for one this page does
 * not colour.
 *
 * The info string is not only a language: CommonMark puts the language first
 * and leaves the rest to whatever the fence is for, so only the first word is
 * read here. The label a reader sees stays the whole of it.
 */
export function fenceLanguage(info: string): string | null {
  const [first] = info.trim().split(/\s+/);
  return first === undefined ? null : namedFor(first);
}

/**
 * The language this page has for `name`, or `null` for one it does not.
 *
 * The lookup is on the table's OWN keys: a name arrives from a fence's info
 * string, which is the model's own text, and `constructor` and `__proto__`
 * resolve on an object literal to the object's own members rather than to
 * nothing.
 */
function namedFor(name: string): string | null {
  const key = name.toLowerCase();
  return Object.hasOwn(NAMED, key) ? (NAMED[key] ?? null) : null;
}

/** `text` with the characters that would end its element escaped. */
function escaped(text: string): string {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/**
 * A block of code as the page draws one: a header naming it, and the text under
 * it coloured.
 *
 * One builder for both ways in - a source file a call read, and a fenced block
 * in a message - so the panel is one piece rather than two drawings of it. The
 * label is what the header prints, and `null` draws no header at all, which is
 * what a fence carrying no info string gets. The language is what the text is
 * coloured as, which is not the label: a fence's info string can name something
 * this page has no grammar for, and the panel still says what the fence called
 * itself.
 */
export function codePanel(label: string | null, language: string | null, text: string): string {
  const header = label === null ? '' : `<div class="lang">${escaped(label)}</div>`;
  return `<div class="code">${header}<pre>${renderCode(language, text)}</pre></div>`;
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
