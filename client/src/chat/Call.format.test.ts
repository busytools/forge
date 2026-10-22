// @vitest-environment jsdom
/**
 * The call row's format rendering: a markdown Read renders, a code Read stays
 * coloured, an Edit keeps its token colours under GitHub-style tinted rows
 * with word tint only on paired changed spans, and a markdown Edit renders
 * with removals struck inline.
 */
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Call from './Call.svelte';
import type { HunkLine, ToolLeaf } from './leaves';

const leaf = (overrides: Partial<ToolLeaf>): ToolLeaf => ({
  id: 'toolu_format',
  row: { kind: 'family', family: 'read' },
  name: 'Read',
  title: 'docs/placement.md',
  command: null,
  status: 'completed',
  backgrounded: false,
  note: null,
  body: [],
  mutation: null,
  decision: null,
  forge: null,
  skill: null,
  image: null,
  imageNote: null,
  ...overrides,
});

/** Mount the row open, let the async highlighter settle, and answer its close. */
async function opened(call: ToolLeaf): Promise<() => void> {
  const app = mount(Call, { target: document.body, props: { k: 'f1', call, open: true } });
  await vi.waitFor(() => {
    flushSync();
    const coloured = document.querySelector(
      '.code pre span[style*="color"], .dif .ln span[style*="color"]',
    );
    if (coloured === null) throw new Error('waiting for the highlighter');
  });
  return () => void unmount(app);
}

const line = (kind: HunkLine['kind'], text: string, old: number | null, next: number | null) => ({
  kind,
  text,
  old,
  new: next,
});

afterEach(() => {
  document.body.innerHTML = '';
});

describe('a Read of markdown', () => {
  it('renders headings and lists instead of drawing the source', () => {
    const app = mount(Call, {
      target: document.body,
      props: {
        k: 'f1',
        open: true,
        call: leaf({
          // The CLI's own shape: every line behind a number and a tab.
          body: [{ kind: 'text', text: '1\t# Placement\n2\t\n3\t- one\n4\t- two\n' }],
        }),
      },
    });
    try {
      flushSync();
      const prose = document.querySelector('.pbody .prose');
      expect(prose?.querySelector('h1')?.textContent, 'the heading renders').toBe('Placement');
      expect(prose?.querySelectorAll('li').length, 'the list renders').toBe(2);
      expect(prose?.textContent, 'no raw markdown marker in the DOM').not.toContain('# Placement');
      expect(document.querySelector('.code'), 'and not the source panel').toBeNull();
    } finally {
      void unmount(app);
    }
  });
});

describe('a Read of an extensionless script', () => {
  /**
   * **The shebang must survive the CLI's gutter.** The row reads the format
   * from the file with the `N<TAB>` numbers off, and hands the grammar to the
   * panel; re-detecting on the numbered text reads no shebang and draws it
   * plain while the file is a script.
   */
  it('reads its shebang through the gutter and colours the panel', async () => {
    const close = await opened(
      leaf({
        title: 'bin/run',
        body: [{ kind: 'text', text: '1\t#!/usr/bin/env bash\n2\techo hi\n' }],
      }),
    );
    try {
      expect(document.querySelector('.code .lang')?.textContent, 'the shebang names it').toBe(
        'shellscript',
      );
      expect(
        document.querySelector('.code pre span[style*="color"]'),
        'and the panel is coloured',
      ).not.toBeNull();
    } finally {
      close();
    }
  });

  /** A filename with no extension still carries its grammar. */
  it('names a Justfile grammar from its filename', async () => {
    const close = await opened(
      leaf({
        title: 'Justfile',
        body: [{ kind: 'text', text: '1\tset shell := ["bash", "-cu"]\n' }],
      }),
    );
    try {
      expect(document.querySelector('.code .lang')?.textContent).toBe('just');
    } finally {
      close();
    }
  });
});

describe('a Read of code', () => {
  it('keeps the coloured panel and its grammar', async () => {
    const close = await opened(
      leaf({ title: 'crates/session.rs', body: [{ kind: 'text', text: 'fn main() {}' }] }),
    );
    try {
      expect(document.querySelector('.code .lang')?.textContent, 'the grammar names it').toBe(
        'rust',
      );
      expect(
        document.querySelector('.code pre span[style*="color"]'),
        'the tokens are coloured',
      ).not.toBeNull();
    } finally {
      close();
    }
  });
});

describe('an Edit of code', () => {
  const edit = leaf({
    row: { kind: 'family', family: 'edit' },
    name: 'Edit',
    title: 'crates/session.rs',
    body: [
      {
        kind: 'hunk',
        header: '@@ -41,2 +41,3 @@',
        lines: [
          line('ctx', 'fn open() {', 41, 41),
          line('del', '    self.owner = "client";', 42, null),
          line('add', '    self.owner = "forge";', null, 42),
          line('add', '    self.extra = 1;', null, 43),
        ],
      },
    ],
    mutation: { hunks: 1, added: 2, removed: 1, all: false, outside: false, omitted: null },
  });

  it('tints the rows and keeps the tokens coloured under the tint', async () => {
    const close = await opened(edit);
    try {
      expect(document.querySelectorAll('.dif .ln.d').length, 'a removed row').toBe(1);
      expect(document.querySelectorAll('.dif .ln.a').length, 'two added rows').toBe(2);
      expect(
        document.querySelector('.dif .ln.d span[style*="color"]'),
        'removed tokens coloured',
      ).not.toBeNull();
      expect(
        document.querySelector('.dif .ln.a span[style*="color"]'),
        'added tokens coloured',
      ).not.toBeNull();
    } finally {
      close();
    }
  });

  it('puts the word tint only on the paired changed span', async () => {
    const close = await opened(edit);
    try {
      const removed = [...document.querySelectorAll('.dif .ln.d .w')].map(
        (span) => span.textContent,
      );
      const added = [...document.querySelectorAll('.dif .ln.a .w')].map((span) => span.textContent);
      expect(removed, 'the removed word is tinted').toEqual(['client']);
      expect(added, 'and only its paired added word').toEqual(['forge']);

      const lastAdd = document.querySelector('.dif .ln.a:last-of-type');
      expect(lastAdd?.querySelector('.w'), 'the unpaired added line has no word tint').toBeNull();
      expect(
        document.querySelector('.dif .ln.ctx .w'),
        'a context line has no word tint',
      ).toBeNull();

      const tinted = document.querySelector('.dif .ln.d .w');
      expect(
        tinted?.closest('.dif')?.querySelector('.ln.d span[style*="color"]'),
        'the word tint sits inside the token colours rather than replacing them',
      ).not.toBeNull();
    } finally {
      close();
    }
  });
});

describe('an Edit of markdown', () => {
  const edit = leaf({
    row: { kind: 'family', family: 'edit' },
    name: 'Edit',
    title: 'docs/book/src/index.md',
    body: [
      {
        kind: 'hunk',
        header: '@@ -1,4 +1,4 @@',
        lines: [
          line('del', '# Forge', 1, null),
          line('add', '# forge - the agent', null, 1),
          line('ctx', '', 2, 2),
          line('del', 'What it is: a terminal', 3, null),
          line('add', 'What it is: a server and its clients', null, 3),
        ],
      },
    ],
    mutation: { hunks: 1, added: 2, removed: 2, all: false, outside: false, omitted: null },
  });

  it('renders the hunk with removals struck inline and additions normal', () => {
    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      const prose = document.querySelector('.pbody .prose');
      expect(prose?.querySelector('h1'), 'the heading renders as a heading').not.toBeNull();

      const struck = [...(prose?.querySelectorAll('.mddel') ?? [])].map((span) => span.textContent);
      const added = [...(prose?.querySelectorAll('.mdadd') ?? [])].map((span) => span.textContent);
      expect(struck, 'the removed words are kept, inline').toEqual(['Forge', 'terminal']);
      expect(added, 'and the added words draw beside them').toEqual([
        'forge - the agent',
        'server and its clients',
      ]);
      expect(document.querySelector('.dif'), 'the source diff is not shown under it').toBeNull();
    } finally {
      void unmount(app);
    }
  });

  it('strikes the removals and leaves the additions alone in the sheet', () => {
    const sheet = readFileSync('src/assets/web.css', 'utf8');
    expect(sheet, 'removed markdown is struck').toMatch(
      /\.prose \.mddel \{[^}]*text-decoration:\s*line-through/,
    );
    expect(sheet, 'added markdown is not').not.toMatch(
      /\.prose \.mdadd \{[^}]*text-decoration:\s*line-through/,
    );
  });
});
