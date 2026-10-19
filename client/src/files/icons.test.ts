import { describe, expect, it } from 'vitest';

import { fileIcon, folderIcon } from './icons';

/**
 * Whether `url` resolves to `file`. The dev transform keeps its `no-inline`
 * query on the URL, and a build emits a hashed asset path, so only the file
 * name is common to both.
 */
const is = (url: string | null, file: string): boolean =>
  (url?.split('?')[0] ?? '').endsWith(`/${file}.svg`);

describe("the files tree's icons", () => {
  it('picks an exact filename before its extension', () => {
    expect(is(fileIcon('CLAUDE.md'), 'claude'), 'CLAUDE.md has its own glyph').toBe(true);
    expect(is(fileIcon('justfile'), 'just'), 'so does justfile').toBe(true);
    expect(is(fileIcon('.gitignore'), 'git'), 'and .gitignore').toBe(true);
    expect(is(fileIcon('Cargo.toml'), 'toml'), 'no exact entry, so the extension').toBe(true);
    expect(is(fileIcon('docs/README.md'), 'markdown'), 'by the extension, in a folder').toBe(true);
  });

  it('tries the longest dotted suffix before the shortest', () => {
    expect(is(fileIcon('a.ts.map'), 'json'), 'ts.map is its own extension in the manifest').toBe(
      true,
    );
    expect(is(fileIcon('a.zzz'), 'file'), 'an unknown extension takes the file glyph').toBe(true);
    expect(is(fileIcon('Makefile.zzz'), 'file'), 'a dotted unknown takes it too').toBe(true);
  });

  it('draws a folder in its open or shut variant, and a known name first', () => {
    expect(is(folderIcon('src', false), 'folder-src'), 'src shut').toBe(true);
    expect(is(folderIcon('src', true), 'folder-src-open'), 'src open').toBe(true);
    expect(is(folderIcon('.github', false), 'folder-github'), 'a dot-named folder').toBe(true);
    expect(is(folderIcon('mockups', false), 'folder'), 'an unnamed folder falls back').toBe(true);
    expect(is(folderIcon('mockups', true), 'folder-open'), 'and its open variant').toBe(true);
  });

  it('draws the project row as an open folder, by its name', () => {
    expect(is(folderIcon('forge', true), 'folder-open'), 'an unnamed project').toBe(true);
    expect(is(folderIcon('src', true), 'folder-src-open'), 'a named one keeps its own').toBe(true);
  });
});
