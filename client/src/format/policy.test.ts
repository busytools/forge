import { describe, expect, it } from 'vitest';

import { formatOf } from './policy';

/** `text` as the bytes a read or a call body would carry. */
const bytes = (text: string): Uint8Array => new TextEncoder().encode(text);

/** Bytes no UTF-8 decoder accepts: the shape of a picture or an archive. */
const binary = (): Uint8Array => new Uint8Array([0xff, 0xfe, 0x00, 0x01]);

describe('what a path is, by extension', () => {
  it('names markdown, pdf, and the picture kinds with their mime', () => {
    expect(formatOf('docs/placement.md'), 'markdown').toEqual({ kind: 'markdown' });
    expect(formatOf('README.markdown'), 'the other markdown extension').toEqual({
      kind: 'markdown',
    });
    expect(formatOf('docs/retro.pdf'), 'a PDF').toEqual({ kind: 'pdf' });
    expect(formatOf('assets/mark.png'), 'a png').toEqual({ kind: 'image', mime: 'image/png' });
    expect(formatOf('a/photo.JPG'), 'case-folded, and the jpeg spelling').toEqual({
      kind: 'image',
      mime: 'image/jpeg',
    });
    expect(formatOf('mark.svg'), 'an svg is drawn as a picture').toEqual({
      kind: 'image',
      mime: 'image/svg+xml',
    });
  });

  it('names code by shiki grammar id, the extension through shiki own aliases', () => {
    expect(formatOf('crates/x/lib.rs'), 'rs is shiki alias for rust').toEqual({
      kind: 'code',
      language: 'rust',
    });
    expect(formatOf('a/x.ts'), 'ts resolves to typescript').toEqual({
      kind: 'code',
      language: 'typescript',
    });
    expect(formatOf('a/x.js'), 'js resolves to javascript').toEqual({
      kind: 'code',
      language: 'javascript',
    });
    expect(formatOf('App.svelte'), 'svelte').toEqual({ kind: 'code', language: 'svelte' });
    expect(formatOf('Cargo.toml'), 'toml').toEqual({ kind: 'code', language: 'toml' });
    expect(formatOf('deploy.sh'), 'sh resolves to shellscript').toEqual({
      kind: 'code',
      language: 'shellscript',
    });
    expect(formatOf('d.tsx'), 'tsx').toEqual({ kind: 'code', language: 'tsx' });
    expect(formatOf('a.htm'), 'htm resolves to html').toEqual({ kind: 'code', language: 'html' });
  });

  it('draws a txt file as text, not as a language named txt', () => {
    expect(formatOf('notes.txt', bytes('hello'))).toEqual({ kind: 'text' });
  });
});

describe('a filename with no useful extension', () => {
  it('names the grammar its own name carries', () => {
    expect(formatOf('Justfile', bytes('set shell := ["bash", "-cu"]'))).toEqual({
      kind: 'code',
      language: 'just',
    });
    expect(formatOf('Makefile', bytes('check:\n\tcargo fmt'))).toEqual({
      kind: 'code',
      language: 'make',
    });
    expect(formatOf('GNUmakefile', bytes('check:'))).toEqual({ kind: 'code', language: 'make' });
    expect(formatOf('Dockerfile', bytes('FROM rust'))).toEqual({
      kind: 'code',
      language: 'docker',
    });
    expect(formatOf('Cargo.lock', bytes('version = 4'))).toEqual({
      kind: 'code',
      language: 'toml',
    });
  });

  it('names a dotenv file by its full name, not by a `.env` extension', () => {
    expect(formatOf('.env', bytes('TOKEN=x'))).toEqual({ kind: 'code', language: 'dotenv' });
    expect(formatOf('.env.local', bytes('TOKEN=x'))).toEqual({ kind: 'code', language: 'dotenv' });
  });

  /**
   * **Shiki ships no `ignore` grammar.** The plan named one, and it does not
   * exist, so these two draw as text with the header note. Pinned by name so
   * nobody closes the gap by inventing a grammar id the highlighter cannot
   * load: `formatOf` resolves against shiki's own metadata, and a name that
   * is not there is text.
   */
  it('draws a dotfile with no grammar as plain text, because no ignore grammar exists', () => {
    expect(formatOf('.gitignore', bytes('target/'))).toEqual({ kind: 'text' });
    expect(formatOf('.dockerignore', bytes('target/'))).toEqual({ kind: 'text' });
  });
});

describe('a script that names its language in its first line', () => {
  it('reads the interpreter through env', () => {
    expect(formatOf('bin/run', bytes('#!/usr/bin/env bash\nset -e'))).toEqual({
      kind: 'code',
      language: 'shellscript',
    });
    expect(formatOf('bin/run', bytes('#!/usr/bin/env node\nconsole.log(1)'))).toEqual({
      kind: 'code',
      language: 'javascript',
    });
  });

  it('reads the interpreter directly', () => {
    expect(formatOf('bin/tool', bytes('#!/usr/bin/python3\nprint(1)'))).toEqual({
      kind: 'code',
      language: 'python',
    });
    expect(formatOf('bin/tool', bytes('#!/usr/bin/ruby\nputs 1'))).toEqual({
      kind: 'code',
      language: 'ruby',
    });
    expect(formatOf('bin/tool', bytes('#!/usr/bin/perl\nprint 1'))).toEqual({
      kind: 'code',
      language: 'perl',
    });
  });

  it('does not guess from anything but the first line', () => {
    expect(formatOf('notes.txt', bytes('a line\n#!/usr/bin/env bash\n'))).toEqual({
      kind: 'text',
    });
  });

  /**
   * **The extension wins over a shebang, and `#!` must open the file.** A
   * `.rs` script with a shell shebang is still Rust; a line that merely
   * contains `#!`, or starts with a space before it, names nothing.
   */
  it('keeps the extension grammar over a shebang, and reads only an exact opener', () => {
    expect(formatOf('tools/build.rs', bytes('#!/usr/bin/env bash\nfn main() {}'))).toEqual({
      kind: 'code',
      language: 'rust',
    });
    expect(formatOf('notes', bytes(' #!/usr/bin/env bash\nset -e'))).toEqual({ kind: 'text' });
    expect(formatOf('notes', bytes('#!/usr/bin/env klingon\nqapla'))).toEqual({ kind: 'text' });
  });
});

describe('everything else', () => {
  it('a UTF-8 file no table names is plain text', () => {
    expect(formatOf('data/notes.weird', bytes('nothing matches a grammar'))).toEqual({
      kind: 'text',
    });
    expect(formatOf('LICENSE', bytes('MIT'))).toEqual({ kind: 'text' });
    expect(formatOf('data/notes.weird'), 'no bytes to check is still text').toEqual({
      kind: 'text',
    });
  });

  it('bytes no decoder accepts are binary, with the mime the extension names', () => {
    expect(formatOf('data/notes.weird', binary())).toEqual({
      kind: 'binary',
      mime: 'application/octet-stream',
    });
    expect(formatOf('dist/forge.tar.gz', binary())).toEqual({
      kind: 'binary',
      mime: 'application/gzip',
    });
    expect(formatOf('bundle.zip', binary())).toEqual({
      kind: 'binary',
      mime: 'application/zip',
    });
  });

  it('a text-shaped extension whose bytes are not UTF-8 is binary, not a broken render', () => {
    expect(formatOf('broken.md', binary())).toEqual({
      kind: 'binary',
      mime: 'text/markdown',
    });
    expect(formatOf('broken.rs', binary())).toEqual({
      kind: 'binary',
      mime: 'text/plain',
    });
  });
});
