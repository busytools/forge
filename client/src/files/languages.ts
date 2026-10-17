/**
 * Which language a path is, for the files page's reader.
 *
 * **Deliberately its own tiny module**: the reader's machinery (CodeMirror,
 * shiki and their grammars) is one lazily-imported chunk, and the page needs
 * these two answers - what to call a path, and whether a read's bytes are
 * text at all - BEFORE it decides to pull that chunk in.
 */

/** One language: its shiki grammar id, and a CodeMirror package where the
 *  org ships one (structure and folding; svelte and toml have none). The
 *  CodeMirror loaders live with the reader (`reader.ts`), which is what
 *  actually loads them. */
export interface Language {
  shiki: string;
  /** Whether a CodeMirror language package exists for the folding layer. */
  cm: boolean;
}

const LANGS: Record<string, Language> = {
  rs: { shiki: 'rust', cm: true },
  ts: { shiki: 'typescript', cm: true },
  mts: { shiki: 'typescript', cm: true },
  cts: { shiki: 'typescript', cm: true },
  tsx: { shiki: 'tsx', cm: true },
  js: { shiki: 'javascript', cm: true },
  mjs: { shiki: 'javascript', cm: true },
  cjs: { shiki: 'javascript', cm: true },
  jsx: { shiki: 'jsx', cm: true },
  py: { shiki: 'python', cm: true },
  go: { shiki: 'go', cm: true },
  json: { shiki: 'json', cm: true },
  yaml: { shiki: 'yaml', cm: true },
  yml: { shiki: 'yaml', cm: true },
  css: { shiki: 'css', cm: true },
  html: { shiki: 'html', cm: true },
  htm: { shiki: 'html', cm: true },
  xml: { shiki: 'xml', cm: true },
  svg: { shiki: 'xml', cm: false },
  sql: { shiki: 'sql', cm: true },
  md: { shiki: 'markdown', cm: true },
  markdown: { shiki: 'markdown', cm: true },
  svelte: { shiki: 'svelte', cm: false },
  toml: { shiki: 'toml', cm: false },
  ini: { shiki: 'ini', cm: false },
  sh: { shiki: 'shellscript', cm: false },
  bash: { shiki: 'shellscript', cm: false },
  zsh: { shiki: 'shellscript', cm: false },
  kt: { shiki: 'kotlin', cm: false },
  kts: { shiki: 'kotlin', cm: false },
  txt: { shiki: 'text', cm: false },
};

/** The language a path names, or `null` for one this page does not colour
 *  (its text still draws, plain). */
export function languageForPath(path: string): Language | null {
  const cut = path.lastIndexOf('.');
  if (cut === -1) return null;
  return LANGS[path.slice(cut + 1).toLowerCase()] ?? null;
}

/**
 * Whether the reader draws a read's bytes as text at all. A code or text
 * kind draws (`reviewable` is the read's own answer, widened to code in the
 * seat-tree cut); a kind it did not call reviewable is not text this page
 * renders - an image read's bytes are not a drawing to invent.
 */
export function drawableText(
  kind: 'markdown' | 'text' | 'pdf' | { other: string } | null,
  reviewable: boolean,
): boolean {
  if (kind === null || kind === 'pdf') return false;
  if (kind === 'markdown' || kind === 'text') return true;
  return reviewable;
}
