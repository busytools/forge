import { describe, expect, it } from 'vitest';

import { leafOf } from './leaves';

/** A result block as the wire shapes one, which is where a tool's answer arrives. */
const answered = (content: string): { type: string; content: string } => ({
  type: 'tool_result',
  content,
});

/** What the CLI writes on a mutation, addressed to the model rather than to a reader. */
const NOTE =
  'File created successfully at a.rs (file state is current in your context \u{2014} no need to Read it back)';

describe('what a call body draws', () => {
  it("draws the CLI's own hunk where the result carries one, marks included", () => {
    // What the wire says about the change: the range it covers, and the lines
    // with a space for context, `-` for removed, `+` for added. Without it the
    // row is two sides with nothing saying where in the file they sit.
    const leaf = leafOf(
      't1',
      'Edit',
      {
        file_path: '/x/a.rs',
        old_string: 'flex: none;',
        new_string: 'flex: 0 1 auto;',
        replace_all: true,
      },
      answered(NOTE),
      {
        userModified: true,
        structuredPatch: [
          {
            oldStart: 30,
            oldLines: 7,
            newStart: 30,
            newLines: 9,
            lines: [' display: flex;', '-  flex: none;', '+  flex: 0 1 auto;', ' }'],
          },
        ],
      },
    );

    const [hunk] = leaf.body;
    expect(hunk?.kind, 'the hunk is what the row draws, not the two sides').toBe('hunk');
    if (hunk?.kind !== 'hunk') throw new Error('the row drew no hunk');
    expect(hunk.header, 'the range the hunk covers, before and after').toBe('@@ -30,7 +30,9 @@');
    expect(
      hunk.lines.map((line) => line.kind),
      'each line read by its mark',
    ).toEqual(['ctx', 'del', 'add', 'ctx']);
    expect(hunk.lines[0]?.text, 'context keeps its own indentation').toBe(' display: flex;');
    expect(hunk.lines[1]?.text, 'and a change loses its mark, which the row draws itself').toBe(
      '  flex: none;',
    );
    expect(leaf.mutation, 'and what the line under it counts, with the two marks').toEqual({
      hunks: 1,
      added: 1,
      removed: 1,
      all: true,
      outside: true,
    });
  });

  it('draws a mutation as its diff, without the note the CLI writes to the model', () => {
    const path = '/Users/x/project/a.rs';
    const leaf = leafOf(
      't1',
      'Edit',
      { file_path: path, old_string: 'one', new_string: 'two' },
      answered(NOTE),
    );

    // The row names the file the wire named, whole: the same path is on the
    // diff under it, and a row that shortened one of them would be naming two
    // files. The signature is the enforcement - `leafOf` is handed no working
    // tree to cut against - and this is what a reader sees of it.
    expect(leaf.title, 'the title is the path the call carried').toBe(path);

    expect(
      leaf.body.map((part) => part.kind),
      'the diff, and nothing after it',
    ).toEqual(['diff']);
    expect(JSON.stringify(leaf.body), 'and the note never reaches the body').not.toContain(
      'no need to Read it back',
    );
    expect(leaf.status, 'the call still settles on the result it drew').toBe('completed');

    // The text is what a body has where there is no diff to draw: the terminal's
    // own rule is the diff for a mutation, the result's words for everything else.
    // A Write is a file the CLI reports as created, with no patch at all: the
    // body falls back to the call's own content, which is the ADDED side and
    // nothing removed. Read the other way round the counts invert.
    const wrote = leafOf(
      't3',
      'Write',
      { file_path: '/x/a.rs', content: 'one\ntwo\n' },
      answered('File created successfully at /x/a.rs'),
      { type: 'create', structuredPatch: [] },
    );
    expect(wrote.mutation, 'a create adds, and removes nothing').toMatchObject({
      hunks: 1,
      added: 3,
      removed: 0,
    });

    const bash = leafOf('t2', 'Bash', { command: 'ls' }, answered('a.rs\nb.rs'));
    expect(
      bash.body.map((part) => part.kind),
      'a call with no diff still says what it said',
    ).toEqual(['text']);
  });
});
