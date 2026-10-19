import { describe, expect, it, vi } from 'vitest';

import { fileIcon, folderIcon, folderIconName, rootIcon } from './icons';

/**
 * Whether `url` resolves to `file`. The dev transform keeps its `no-inline`
 * query on the URL, a build emits a hashed asset path and a cached glyph is
 * an object URL, so only the file name is common to all of them.
 */
const is = (url: string | null, file: string): boolean =>
  (url?.split('?')[0] ?? '').endsWith(`/${file}.svg`);

describe("the files tree's icons", () => {
  it('matches a parent-qualified or exact filename before the extension, any case', () => {
    expect(is(fileIcon('CLAUDE.md'), 'claude'), 'CLAUDE.md').toBe(true);
    expect(is(fileIcon('README.md'), 'readme'), 'README.md, by its exact name').toBe(true);
    expect(is(fileIcon('Dockerfile'), 'docker'), 'Dockerfile').toBe(true);
    expect(is(fileIcon('Makefile'), 'makefile'), 'Makefile').toBe(true);
    expect(is(fileIcon('LICENSE'), 'license'), 'LICENSE').toBe(true);
    expect(is(fileIcon('Gemfile'), 'gemfile'), 'Gemfile').toBe(true);
    expect(is(fileIcon('Justfile'), 'just'), 'Justfile, any case').toBe(true);
    expect(is(fileIcon('CMakeLists.txt'), 'cmake'), 'CMakeLists.txt').toBe(true);
    expect(is(fileIcon('.gitignore'), 'git'), '.gitignore').toBe(true);
    expect(is(fileIcon('Cargo.toml'), 'toml'), 'no exact entry, so the extension').toBe(true);
    expect(is(fileIcon('docs/notes.md'), 'markdown'), 'the extension alone').toBe(true);
    expect(is(fileIcon('app.PNG'), 'image'), 'an upper-case extension').toBe(true);
    expect(is(fileIcon('index.HTML'), 'html'), 'and another').toBe(true);
  });

  it('matches a parent-qualified name before the bare one', () => {
    expect(is(fileIcon('.github/FUNDING.yml'), 'github-sponsors'), 'a parent-qualified file').toBe(
      true,
    );
    expect(
      is(fileIcon('repo/.github/FUNDING.yml'), 'github-sponsors'),
      'nested, by its immediate parent',
    ).toBe(true);
  });

  it('tries the longest dotted suffix before the shortest', () => {
    expect(is(fileIcon('foo.d.ts'), 'typescript-def'), 'd.ts is its own extension').toBe(true);
    expect(is(fileIcon('a.ts.map'), 'json'), 'ts.map is its own extension').toBe(true);
    expect(is(fileIcon('a.zzz'), 'file'), 'an unknown extension takes the file glyph').toBe(true);
    expect(is(fileIcon('Makefile.zzz'), 'file'), 'a dotted unknown takes it too').toBe(true);
  });

  it('draws a folder in its open or shut variant, by name and parent, any case', () => {
    expect(is(folderIcon('src', false), 'folder-src'), 'src shut').toBe(true);
    expect(is(folderIcon('src', true), 'folder-src-open'), 'src open').toBe(true);
    expect(is(folderIcon('.github', false), 'folder-github'), 'a dot-named folder').toBe(true);
    expect(is(folderIcon('a/META-INF', false), 'folder-config'), 'an upper-case name').toBe(true);
    expect(
      is(folderIcon('.github/workflows', false), 'folder-gh-workflows'),
      'a parent-qualified folder',
    ).toBe(true);
    expect(
      is(folderIcon('.github/ISSUE_TEMPLATE', true), 'folder-template-open'),
      'an upper-case parent-qualified folder',
    ).toBe(true);
    expect(is(folderIcon('mockups', false), 'folder'), 'an unnamed folder falls back').toBe(true);
    expect(is(folderIcon('mockups', true), 'folder-open'), 'and its open variant').toBe(true);
  });

  it('keeps a named shut glyph for an open folder with no expanded entry', () => {
    expect(
      folderIconName('folder-src', undefined, 'folder', 'folder-open', true),
      'VS Code keeps the shut name',
    ).toBe('folder-src');
    expect(
      folderIconName('folder-src', 'folder-src-open', 'folder', 'folder-open', true),
      'an expanded name wins when it exists',
    ).toBe('folder-src-open');
    expect(
      folderIconName(undefined, undefined, 'folder', 'folder-open', true),
      'no name at all takes the kind fallback',
    ).toBe('folder-open');
  });

  it('draws the project row with the theme root folder, keyed by its path', () => {
    expect(
      is(rootIcon('/Users/me/proj/.claude/worktrees/files-fold'), 'folder-root-open'),
      'the root folder, open',
    ).toBe(true);
  });

  it('fetches a glyph once, however many rows mount it', async () => {
    vi.resetModules();
    const fetchMock = vi.fn(() =>
      Promise.resolve({
        ok: true,
        status: 200,
        blob: () => Promise.resolve(new Blob(['<svg/>'], { type: 'image/svg+xml' })),
      }),
    );
    vi.stubGlobal('fetch', fetchMock);
    vi.stubGlobal(
      'URL',
      class extends URL {
        static createObjectURL = (): string => 'blob:glyph';
      },
    );
    try {
      const fresh = await import('./icons');
      expect(is(fresh.fileIcon('src/main.rs'), 'rust'), 'the bundled asset draws at once').toBe(
        true,
      );
      await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
      await vi.waitFor(() =>
        expect(fresh.fileIcon('src/main.rs'), 'the object URL takes over').toBe('blob:glyph'),
      );
      fresh.fileIcon('src/other.rs');
      expect(fetchMock, 'a second row of the same type fetches nothing new').toHaveBeenCalledTimes(
        1,
      );
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
