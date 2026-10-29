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
  stop: null,
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
  // A COLD shiki chunk costs seconds on CI; the default wait is too short.
  await vi.waitFor(
    () => {
      flushSync();
      const coloured = document.querySelector(
        '.code pre span[style*="color"], .dif .ln span[style*="color"]',
      );
      if (coloured === null) throw new Error('waiting for the highlighter');
    },
    { timeout: 10_000 },
  );
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
      const prose = document.querySelector('.code .code-diff .prose');
      expect(document.querySelector('.code > .lang')?.textContent, 'the format names it').toBe(
        'markdown',
      );
      expect(prose?.querySelector('h1')?.textContent, 'the heading renders').toBe('Placement');
      expect(prose?.querySelectorAll('li').length, 'the list renders').toBe(2);
      expect(prose?.textContent, 'no raw markdown marker in the DOM').not.toContain('# Placement');
      expect(document.querySelector('.code > pre'), 'and not the source panel').toBeNull();
    } finally {
      void unmount(app);
    }
  });
});

describe('an Edit of a text file', () => {
  /**
   * **No grammar means no format strip.** The strip names a format the page
   * knows; a text file must not be labelled `text` as if that were one, the
   * same rule Code.svelte follows for a Read.
   */
  it('draws the panel with no format strip', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'notes.weird',
      body: [
        {
          kind: 'hunk',
          header: '@@ -1,1 +1,2 @@',
          lines: [
            line('ctx', 'nothing here matches a grammar', 1, 1),
            line('add', 'and this line does not either', null, 2),
          ],
        },
      ],
      mutation: { hunks: 1, added: 1, removed: 0, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.code.panel'), 'the panel still draws').not.toBeNull();
      expect(
        document.querySelector('.code > .lang'),
        'a file with no grammar gets no format strip',
      ).toBeNull();
    } finally {
      void unmount(app);
    }
  });
});

describe('an Edit of an extensionless script', () => {
  /**
   * **The format strip is read from the change, not the path.** An
   * extensionless script names its language only in its shebang, so a
   * path-only probe would leave the panel unnamed.
   */
  it('reads the shebang from the change and names the panel', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'bin/run',
      body: [
        {
          kind: 'hunk',
          header: '@@ -0,0 +1,2 @@',
          lines: [
            line('add', '#!/usr/bin/env bash', null, 1),
            line('add', 'set -euo pipefail', null, 2),
          ],
        },
      ],
      mutation: { hunks: 1, added: 2, removed: 0, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.code > .lang')?.textContent, 'the shebang names it').toBe(
        'shellscript',
      );
    } finally {
      void unmount(app);
    }
  });

  /**
   * **A slice that starts past line 1 cannot know the file's first line.** A
   * shebang-shaped line there is data, not the script's interpreter.
   */
  it('draws no strip for a hunk that starts past the file head', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'bin/run',
      body: [
        {
          kind: 'hunk',
          header: '@@ -10,1 +10,1 @@',
          lines: [line('ctx', '#!/usr/bin/env bash', 10, 10)],
        },
      ],
      mutation: { hunks: 1, added: 0, removed: 0, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.code > .lang'), 'nothing to name it by').toBeNull();
    } finally {
      void unmount(app);
    }
  });

  /** The NEW side decides on the `diff` fallback too, not only in a hunk. */
  it('draws no strip when a diff fallback empties the shebang', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'bin/run',
      body: [{ kind: 'diff', old: '#!/usr/bin/env bash\nset -e', new: '' }],
      mutation: { hunks: 1, added: 0, removed: 2, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.code.panel'), 'the panel still draws').not.toBeNull();
      expect(
        document.querySelector('.code > .lang'),
        'the emptied new side names nothing',
      ).toBeNull();
    } finally {
      void unmount(app);
    }
  });

  /** The NEW side decides: a removed shebang does not name the panel. */
  it('draws no strip when the change removes the shebang', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'bin/run',
      body: [
        {
          kind: 'hunk',
          header: '@@ -1,1 +1,1 @@',
          lines: [line('del', '#!/usr/bin/env bash', 1, null), line('add', 'set -e', null, 1)],
        },
      ],
      mutation: { hunks: 1, added: 1, removed: 1, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.code > .lang'), 'the new file is no script').toBeNull();
    } finally {
      void unmount(app);
    }
  });
});

describe('a Read of markdown with a fence', () => {
  /**
   * **A fence inside the rendered body is chrome-less.** The outer panel owns
   * the border, ground and strip; the fence renderer's own panel would
   * otherwise draw a box inside a box, and prose keeps the colour and
   * paragraph gap the old lane gave it.
   */
  it('flattens the fence panel inside the outer one and keeps the prose tone', () => {
    const app = mount(Call, {
      target: document.body,
      props: {
        k: 'f1',
        open: true,
        call: leaf({
          body: [
            {
              kind: 'text',
              text: '1\t# Title\n2\t\n3\t```rust\n4\tfn main() {}\n5\t```\n',
            },
          ],
        }),
      },
    });
    try {
      flushSync();
      const outer = document.querySelector('.code.panel');
      expect(outer, 'the body is one panel').not.toBeNull();
      expect(
        outer?.querySelector('.code-diff .prose'),
        'the markdown renders inside',
      ).not.toBeNull();
      expect(
        outer?.querySelector('.code-diff .code'),
        'the fence still draws its own code block inside',
      ).not.toBeNull();

      const sheet = readFileSync('src/assets/web.css', 'utf8');
      expect(sheet, 'the inner fence drops its border').toMatch(
        /\.code-diff \.code \{[^}]*border: 0/,
      );
      expect(sheet, 'and its language strip').toMatch(
        /\.code-diff \.code \.lang \{[^}]*display: none/,
      );
      expect(sheet, 'the prose keeps its old colour').toMatch(
        /\.code-diff \{[^}]*color: var\(--muted\)/,
      );
      expect(sheet, 'and its old paragraph gap').toMatch(
        /\.code-diff \.prose p \+ p \{ margin-top: 9px; \}/,
      );
      expect(sheet, 'inline code keeps its old tone and radius').toMatch(
        /\.code-diff code \{[^}]*color: var\(--text\)[^}]*border-radius: 4px/,
      );
      expect(sheet, 'a wide table may outgrow the markdown panel').toContain(
        '.code.markdown-panel { overflow: visible; }',
      );
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
        document.querySelector('.code > .dif'),
        'the diff sits inside the Read panel, not a box of its own',
      ).not.toBeNull();
      expect(document.querySelector('.code > .lang')?.textContent, 'the format names it').toBe(
        'rust',
      );
      expect(
        document.querySelector('.dif .ln.d span[style*="color"]'),
        'removed tokens coloured',
      ).not.toBeNull();
      expect(
        document.querySelector('.dif .ln.a span[style*="color"]'),
        'added tokens coloured',
      ).not.toBeNull();
      // **The panel is a container; the diff's own grounds and word tints are
      // unchanged inside it.** Pinned on the sheet, because jsdom performs no
      // layout and the tint is a background rather than a drawn box.
      const sheet = readFileSync('src/assets/web.css', 'utf8');
      expect(sheet, 'the row grounds').toMatch(/\.dif \{[^}]*--add-bg[^}]*--del-bg/);
      expect(sheet, 'the added row ground').toContain(
        '.dif .ln.a .l { background: var(--add-bg); }',
      );
      expect(sheet, 'the removed row ground').toContain(
        '.dif .ln.d .l { background: var(--del-bg); }',
      );
      expect(sheet, 'the added word tint').toContain(
        '.dif .ln.a .w { background: var(--add-word);',
      );
      expect(sheet, 'the removed word tint').toContain(
        '.dif .ln.d .w { background: var(--del-word);',
      );
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

      // The tint must be INSIDE the token colour: a `.w` that replaced the
      // colour span has an unstyled parent and would pass a page-wide query.
      for (const tint of document.querySelectorAll('.dif .w')) {
        const parent = tint.parentElement?.getAttribute('style') ?? '';
        expect(
          parent,
          `the tint on "${tint.textContent ?? ''}" sits inside a token colour`,
        ).toContain('color:');
      }
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
      const prose = document.querySelector('.code .code-diff .prose');
      expect(prose?.querySelector('h1'), 'the heading renders as a heading').not.toBeNull();
      expect(document.querySelector('.code > .lang')?.textContent, 'the format names it').toBe(
        'markdown',
      );

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

  /**
   * **A whole removed block keeps its marker and its words.** A three-line
   * removal paired with one added heading is the shape a paragraph deletion
   * takes in a hunk; the removed heading has to still parse as a heading,
   * with its words struck, rather than spilling a raw `##` into a paragraph.
   */
  it('draws a whole removed block struck, its heading marker still parsing', () => {
    const block = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'docs/book/src/index.md',
      body: [
        {
          kind: 'hunk',
          header: '@@ -1,5 +1,3 @@',
          lines: [
            line('del', '## The old heading', 1, null),
            line('del', 'the first removed paragraph', 2, null),
            line('del', 'the second removed paragraph', 3, null),
            line('add', '## The kept heading', null, 1),
          ],
        },
      ],
      mutation: { hunks: 1, added: 1, removed: 3, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: block, open: true } });
    try {
      flushSync();
      const prose = document.querySelector('.code .code-diff .prose');
      const headings = [...(prose?.querySelectorAll('h2') ?? [])].map((node) => node.textContent);
      expect(headings, 'both headings parse as headings').toEqual([
        'The old heading',
        'The kept heading',
      ]);
      const struck = [...(prose?.querySelectorAll('.mddel') ?? [])].map((span) => span.textContent);
      expect(struck, 'the removed block keeps every word, struck').toEqual([
        'The old heading',
        'the first removed paragraph',
        'the second removed paragraph',
      ]);
      expect(document.querySelector('.dif'), 'the source diff is not shown').toBeNull();
    } finally {
      void unmount(app);
    }
  });

  /**
   * **A changed reference definition falls back to the source diff.** A
   * rendered view hides definitions, and a sentinel inside one invalidates
   * the definition and every reference to it - so the hunk draws as source.
   */
  it('draws the source diff when a reference definition changes', () => {
    const definitions = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'docs/refs.md',
      body: [
        {
          kind: 'hunk',
          header: '@@ -1,2 +1,2 @@',
          lines: [
            line('del', '[docs]: https://example.com/old', 1, null),
            line('add', '[docs]: https://example.com/new', null, 1),
            line('ctx', 'see [the docs][docs]', 2, 2),
          ],
        },
      ],
      mutation: { hunks: 1, added: 1, removed: 1, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, {
      target: document.body,
      props: { k: 'f1', call: definitions, open: true },
    });
    try {
      flushSync();
      expect(document.querySelector('.dif'), 'the source diff draws').not.toBeNull();
      expect(
        document.querySelector('.dif .ln.d .l')?.textContent,
        'the old definition is shown as source',
      ).toContain('https://example.com/old');
      expect(document.querySelector('.code .code-diff'), 'no rendered diff').toBeNull();
      expect(
        document.querySelector('.code > .lang'),
        'a markdown source diff names no code grammar',
      ).toBeNull();
    } finally {
      void unmount(app);
    }
  });

  /**
   * A `diff` piece is the fallback an Edit builds from its own two sides; a
   * Write's fallback is a hunk from line zero. This is that diff branch.
   */
  /**
   * **A created markdown file renders as the document it is.** Every line of
   * a create is an addition, so marking them drew the whole file green; the
   * row's `+N -0` already says it is new, and the document should read.
   */
  it('renders a created markdown file with no change marks, definitions and all', () => {
    const write = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Write',
      title: 'memory/note.md',
      body: [
        {
          kind: 'hunk',
          header: '@@ -0,0 +1,4 @@',
          lines: [
            line('add', '# A note', null, 1),
            line('add', 'The body links [the docs][docs].', null, 2),
            line('add', '', null, 3),
            line('add', '[docs]: https://example.com', null, 4),
          ],
        },
      ],
      mutation: { hunks: 1, added: 4, removed: 0, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: write, open: true } });
    try {
      flushSync();
      const prose = document.querySelector('.code .code-diff .prose');
      expect(prose?.querySelector('h1')?.textContent, 'the document renders').toBe('A note');
      expect(
        prose?.querySelector('a')?.getAttribute('href'),
        'the definition still resolves and draws no source line',
      ).toBe('https://example.com');
      expect(document.querySelectorAll('.mdadd, .mddel').length, 'no change is marked').toBe(0);
      expect(document.querySelector('.dif'), 'and no source diff is drawn').toBeNull();
    } finally {
      void unmount(app);
    }
  });

  /**
   * **An insertion is not a create.** A pure insertion with no context is
   * all additions too, but it changed an existing file, so its line draws as
   * an addition rather than the file rendering unmarked.
   */
  it('marks an all-addition insertion into an existing markdown file', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'docs/old.md',
      body: [
        {
          kind: 'hunk',
          header: '@@ -12,0 +13,1 @@',
          lines: [line('add', 'A line appended at the end.', null, 13)],
        },
      ],
      mutation: { hunks: 1, added: 1, removed: 0, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      const prose = document.querySelector('.code .code-diff .prose');
      expect(
        [...(prose?.querySelectorAll('.mdadd') ?? [])].map((span) => span.textContent),
        'the inserted line is marked as an addition',
      ).toEqual(['A line appended at the end.']);
    } finally {
      void unmount(app);
    }
  });

  it('renders a markdown change from its two sides', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'docs/old.md',
      body: [{ kind: 'diff', old: '# Old\n\nthe first line', new: '# New\n\nthe first line' }],
      mutation: { hunks: 1, added: 1, removed: 1, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      const prose = document.querySelector('.code .code-diff .prose');
      expect(
        [...(prose?.querySelectorAll('h1') ?? [])].map((heading) => heading.textContent),
        'the changed heading draws both sides as headings',
      ).toEqual(['Old', 'New']);
      expect(
        [...(prose?.querySelectorAll('.mddel') ?? [])].map((span) => span.textContent),
        'the old words are struck',
      ).toEqual(['Old']);
      expect(
        [...(prose?.querySelectorAll('.mdadd') ?? [])].map((span) => span.textContent),
        'the new words are added',
      ).toEqual(['New']);
    } finally {
      void unmount(app);
    }
  });
});

describe('an Edit of code', () => {
  /**
   * **A diff fallback keeps the unchanged lines as context.** The piece
   * carries a replaced region, not a hunk, and drawing every old line as a
   * removal and every new line as an addition showed a whole file changed
   * where one line did.
   */
  it('draws only the changed line of a diff fallback, the rest as context', () => {
    const edit = leaf({
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'client/src/assets/web.css',
      body: [
        {
          kind: 'diff',
          old: '.row {\n  display: flex;\n  flex: none;\n}',
          new: '.row {\n  display: flex;\n  flex: 0 1 auto;\n}',
        },
      ],
      mutation: { hunks: 1, added: 1, removed: 1, all: false, outside: false, omitted: null },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: edit, open: true } });
    try {
      flushSync();
      const rows = [...document.querySelectorAll('.dif .ln')];
      const kinds = rows.map((row) =>
        row.classList.contains('d') ? 'd' : row.classList.contains('a') ? 'a' : 'ctx',
      );
      expect(kinds, 'the shared lines stay context, one line removed and one added').toEqual([
        'ctx',
        'ctx',
        'd',
        'a',
        'ctx',
      ]);
      expect(
        rows
          .filter((row) => row.classList.contains('d'))
          .map((row) => row.querySelector('.l')?.textContent),
        'the old changed line only',
      ).toEqual(['  flex: none;']);
      expect(
        [...document.querySelectorAll('.dif .ln.d .w')].map((word) => word.textContent),
        'the removed word is tinted inside its line',
      ).toEqual(['none']);
      expect(
        [...document.querySelectorAll('.dif .ln.a .w')].map((word) => word.textContent),
        'the added words are tinted',
      ).toEqual(['0 1 auto']);
      expect(
        rows.some((row) => row.classList.contains('ctx') && row.querySelector('.w') !== null),
        'a context line carries no word tint',
      ).toBe(false);
    } finally {
      void unmount(app);
    }
  });
});

describe('a Read of a picture', () => {
  /**
   * **The image's sidecar is text about the call, not the file.** "Saved to
   * ..." beside a screenshot used to be sent to the code panel because its
   * format was not markdown; it invented a plain-text note for a file that
   * was never read as text. It belongs in the plain-text panel.
   */
  it('draws the sidecar in the plain-text panel, not a code panel', () => {
    const shot = leaf({
      title: 'shots/screen.png',
      body: [{ kind: 'text', text: 'Saved to /tmp/screen.png' }],
      image: { mime: 'image/png', data: 'AA==' },
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: shot, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.shot img'), 'the picture draws').not.toBeNull();
      expect(document.querySelector('.code'), 'no code panel for a sidecar').toBeNull();
      expect(
        document.querySelector('.panel.bodytext')?.textContent,
        'the sidecar in its own panel',
      ).toContain('Saved to /tmp/screen.png');
    } finally {
      void unmount(app);
    }
  });

  /**
   * **An image PIECE with no drawn picture is a plain body too.** It used to
   * be the last `.term` in a call body; the hook inset must not reappear
   * here.
   */
  it('draws a bare image piece in the same plain panel', () => {
    const piece = leaf({
      title: 'a result',
      body: [{ kind: 'image', mime: 'image/png', uri: null }],
    });

    const app = mount(Call, { target: document.body, props: { k: 'f1', call: piece, open: true } });
    try {
      flushSync();
      expect(document.querySelector('.term'), 'no hook inset in a call body').toBeNull();
      expect(document.querySelector('.panel.bodytext')?.textContent, 'the caption panel').toContain(
        'image',
      );
    } finally {
      void unmount(app);
    }
  });
});
