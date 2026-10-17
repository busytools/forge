import { describe, expect, it } from 'vitest';

import { LANGS, drawableText, languageForPath } from './languages';

/**
 * **The two language lists are one set named twice.** This literal is the
 * SERVER's list, `crates/forge-agent/src/env/documents.rs`'s
 * `TEXT_EXTENSIONS`, copied by hand because a constant cannot cross the
 * wire - so a language added to `LANGS` is added there in the same change,
 * and the server's own test pins the same set from its side (the lead's
 * gate, 2026-10-17: eight extensions were reviewable here and refused
 * there, because nothing compared the two).
 */
const SERVER_TEXT_EXTENSIONS: string[] = [
  'rs',
  'ts',
  'mts',
  'cts',
  'tsx',
  'js',
  'mjs',
  'cjs',
  'jsx',
  'svelte',
  'py',
  'go',
  'json',
  'toml',
  'yaml',
  'yml',
  'sh',
  'bash',
  'zsh',
  'css',
  'html',
  'htm',
  'xml',
  'svg',
  'sql',
  'kt',
  'kts',
  'md',
  'markdown',
  'ini',
  'txt',
];

describe('the language a path names', () => {
  it("is the server's reviewable set, exactly, in both directions", () => {
    const ours = Object.keys(LANGS).sort();
    const theirs = [...SERVER_TEXT_EXTENSIONS].sort();
    expect(ours, 'LANGS and TEXT_EXTENSIONS are one set named twice').toEqual(theirs);
  });

  it('reads the extension, case-folded, and names the shiki grammar', () => {
    expect(languageForPath('crates/x/src/lib.rs')?.shiki).toBe('rust');
    expect(languageForPath('client/src/App.SVELTE')?.shiki).toBe('svelte');
    expect(languageForPath('Cargo.toml')?.shiki).toBe('toml');
    expect(languageForPath('a/b/c.test.TS')?.shiki).toBe('typescript');
    expect(languageForPath('deploy.sh')?.shiki).toBe('shellscript');
  });

  it('a path with no extension or an unknown one is not coloured', () => {
    expect(languageForPath('LICENSE')).toBe(null);
    expect(languageForPath('assets/logo.png')).toBe(null);
    expect(languageForPath('notes.zzz')).toBe(null);
  });

  it('says which languages have a structure layer and which are colour only', () => {
    expect(languageForPath('x.rs')?.cm).toBe(true);
    expect(languageForPath('x.svelte')?.cm).toBe(false);
    expect(languageForPath('x.toml')?.cm).toBe(false);
  });
});

describe('whether a read draws as text', () => {
  it('markdown and text draw, whatever the flag', () => {
    expect(drawableText('markdown', false)).toBe(true);
    expect(drawableText('text', true)).toBe(true);
  });

  it('a code kind draws when the read called it reviewable', () => {
    expect(drawableText({ other: 'rs' }, true)).toBe(true);
  });

  it('an image read draws nothing, and no read at all draws nothing', () => {
    expect(drawableText({ other: 'png' }, false)).toBe(false);
    expect(drawableText(null, false)).toBe(false);
  });
});
