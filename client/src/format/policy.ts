/**
 * What a file is, and how a surface draws it.
 *
 * **One policy for every surface that reads a file** - the chat's Read and
 * Edit rows and the Files reader - so the same path draws the same way
 * wherever it is opened. Shiki's own language metadata is the table: its
 * bundled ids and aliases name every grammar the highlighter can load, so
 * there is no second list to drift.
 */

import { bundledLanguagesInfo } from 'shiki/langs';

/** One file, as a surface decides how to draw it. */
export type Format =
  | { kind: 'markdown' }
  /** A grammar shiki can load, named by its canonical id. */
  | { kind: 'code'; language: string }
  | { kind: 'text' }
  | { kind: 'image'; mime: string }
  | { kind: 'pdf' }
  /** No viewer: the notice names its type and size. */
  | { kind: 'binary'; mime: string };

/** Canonical shiki grammar id by its bundled id and every alias (`rs` -> `rust`). */
const LANGUAGES: ReadonlyMap<string, string> = new Map(
  bundledLanguagesInfo.flatMap((info) => [
    [info.id, info.id] as const,
    ...(info.aliases ?? []).map((alias) => [alias, info.id] as const),
  ]),
);

const IMAGE_MIME: Readonly<Record<string, string>> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  bmp: 'image/bmp',
  ico: 'image/x-icon',
  svg: 'image/svg+xml',
};

/** The mime a binary's notice names, where the extension names a known type. */
const BINARY_MIME: Readonly<Record<string, string>> = {
  gz: 'application/gzip',
  tgz: 'application/gzip',
  zip: 'application/zip',
  tar: 'application/x-tar',
  bz2: 'application/x-bzip2',
  xz: 'application/x-xz',
  zst: 'application/zstd',
  '7z': 'application/x-7z-compressed',
  wasm: 'application/wasm',
  md: 'text/markdown',
  markdown: 'text/markdown',
};

/** A filename that names its grammar, checked before the UTF-8 fallback. */
const FILENAMES: Readonly<Record<string, string>> = {
  justfile: 'just',
  makefile: 'make',
  gnumakefile: 'make',
  dockerfile: 'docker',
  'cargo.lock': 'toml',
};

/** The interpreters a shebang names, by the word that follows it. */
const SHEBANG: Readonly<Record<string, string>> = {
  sh: 'shellscript',
  bash: 'shellscript',
  zsh: 'shellscript',
  dash: 'shellscript',
  ksh: 'shellscript',
  python: 'python',
  python3: 'python',
  node: 'javascript',
  nodejs: 'javascript',
  ruby: 'ruby',
  perl: 'perl',
};

const MARKDOWN = new Set(['md', 'markdown']);

/** Extensions shiki's aliases do not carry, mapped to the id they mean. */
const EXTENSION_ALIAS: Readonly<Record<string, string>> = { htm: 'html' };

/** The basename, lowercased: what a filename table matches on. */
function baseOf(path: string): string {
  const cut = path.lastIndexOf('/');
  return (cut === -1 ? path : path.slice(cut + 1)).toLowerCase();
}

/** The extension after the last dot of the basename, lowercased, or `''`. */
function extensionOf(base: string): string {
  const cut = base.lastIndexOf('.');
  return cut <= 0 ? '' : base.slice(cut + 1);
}

/** Whether `bytes` decode as UTF-8, which is what makes a file text at all. */
function isText(bytes: Uint8Array): boolean {
  try {
    new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    return true;
  } catch {
    return false;
  }
}

/** The first line's interpreter, when the file opens with a shebang. */
function shebangLanguage(bytes: Uint8Array): string | null {
  let first = '';
  for (const byte of bytes) {
    if (byte === 0x0a) break;
    first += String.fromCharCode(byte);
  }
  if (!first.startsWith('#!')) return null;
  const words = first.slice(2).trim().split(/\s+/);
  let interpreter = words[0]?.split('/').pop() ?? '';
  if (interpreter === 'env') {
    interpreter =
      words
        .slice(1)
        .find((word) => !word.startsWith('-'))
        ?.split('/')
        .pop() ?? '';
  }
  return SHEBANG[interpreter] ?? null;
}

/** The binary notice's mime: the extension's own type, else the generic one. */
function binaryMime(extension: string): string {
  return BINARY_MIME[extension] ?? 'application/octet-stream';
}

/**
 * What `path` is, and how a surface draws it.
 *
 * Order: the extension; the filename; a shebang; then UTF-8 text; anything
 * else is binary. `bytes` is what says whether a text-shaped path really is
 * text - a corrupt markdown file draws as a binary notice rather than as
 * replacement characters.
 *
 * `bytes` is optional because a caller can hold a path without content; a
 * path no table names is then text, which is what such a caller draws.
 */
export function formatOf(path: string, bytes?: Uint8Array | null): Format {
  const base = baseOf(path);
  const extension = extensionOf(base);

  if (extension === 'pdf') return { kind: 'pdf' };
  const image = IMAGE_MIME[extension];
  if (image !== undefined) return { kind: 'image', mime: image };

  const known = bytes == null || isText(bytes);
  if (MARKDOWN.has(extension)) {
    return known ? { kind: 'markdown' } : { kind: 'binary', mime: binaryMime(extension) };
  }
  const language = EXTENSION_ALIAS[extension] ?? LANGUAGES.get(extension);
  if (language !== undefined) {
    return known
      ? { kind: 'code', language }
      : { kind: 'binary', mime: BINARY_MIME[extension] ?? 'text/plain' };
  }

  if (base === '.env' || base.startsWith('.env.')) {
    return known ? { kind: 'code', language: 'dotenv' } : { kind: 'binary', mime: binaryMime('') };
  }
  const named = FILENAMES[base];
  if (named !== undefined) {
    return known ? { kind: 'code', language: named } : { kind: 'binary', mime: binaryMime('') };
  }

  if (bytes == null) return { kind: 'text' };
  if (isText(bytes)) {
    const shebang = shebangLanguage(bytes);
    return shebang === null ? { kind: 'text' } : { kind: 'code', language: shebang };
  }
  return { kind: 'binary', mime: binaryMime(extension) };
}

/** The shiki grammar `path` names, or `null` for a path drawn as text or media. */
export function languageOfPath(path: string, bytes?: Uint8Array | null): string | null {
  const format = formatOf(path, bytes);
  return format.kind === 'code' ? format.language : null;
}
