import { describe, expect, it } from 'vitest';

import { leafOf, opensByDefault, type CallBody } from './leaves';

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
      added: 2,
      removed: 0,
    });
    const [made] = wrote.body;
    expect(made?.kind, 'and it draws as the file it made').toBe('hunk');
    if (made?.kind !== 'hunk') throw new Error('a create drew no hunk');
    expect(made.header, 'from line zero, which is what git writes for a new file').toBe(
      '@@ -0,0 +1,2 @@',
    );

    const bash = leafOf('t2', 'Bash', { command: 'ls' }, answered('a.rs\nb.rs'));
    expect(
      bash.body.map((part) => part.kind),
      'a call with no diff still says what it said',
    ).toEqual(['text']);
  });

  it("keeps a failed mutation's own reason, which is the one thing the result has to say", () => {
    // A successful mutation's result text repeats the path and the CLI's note
    // to the model, which the row drops in favour of the diff. A FAILED one's
    // text is the reason nothing changed, and the terminal draws it (`if
    // !is_error` in `build_tool_result_fields`) - dropped here it is a row
    // that says the edit failed and never says why.
    const failed = leafOf(
      't4',
      'Edit',
      { file_path: '/x/a.rs', old_string: 'one', new_string: 'two' },
      { type: 'tool_result', content: 'String to replace not found in file.', is_error: true },
    );

    expect(
      failed.body.map((part) => part.kind),
      "the diff it meant to make, and the CLI's reason after it",
    ).toEqual(['diff', 'text']);
    expect(
      failed.body.some(
        (part) => part.kind === 'text' && part.text.includes('String to replace not found'),
      ),
      'the reason itself reaches the row',
    ).toBe(true);
    expect(failed.status, 'and the row still settles on the failure').toBe('failed');
  });

  it("reads the CLI's own envelope off a failed result, keeping the words it wrapped", () => {
    // **The wrapper is addressed to the model, not to a reader.** It was drawn
    // raw under the diff (measured on Ved's 2026-10-03 screenshot), and the
    // fold is the one point every result's text enters - so it is read off
    // once here, the same reading the terminal's `extract_tool_use_error_message`
    // gives the same payload. The first line is the message; the rest is detail
    // the note path would drop.
    const wrapped =
      '<tool_use_error>String to replace not found in file.\n' +
      'String:   fn outcome(answer: Answer) {\n' +
      '    AskOutcome { model: "test-model".to_owned() }</tool_use_error>';
    const failed = leafOf(
      't5',
      'Edit',
      { file_path: '/x/a.rs', old_string: 'one', new_string: 'two' },
      { type: 'tool_result', content: wrapped, is_error: true },
    );

    expect(failed.body.at(-1), 'the failure draws as its own piece, wrapper gone').toEqual({
      kind: 'error',
      message: 'String to replace not found in file.',
      detail:
        'String:   fn outcome(answer: Answer) {\n    AskOutcome { model: "test-model".to_owned() }',
    });
    expect(JSON.stringify(failed.body), 'and the tag never reaches the page').not.toContain(
      'tool_use_error',
    );

    // The single-line payload, which is the other shape the envelope arrives
    // in: a message with nothing under it.
    const quiet = leafOf(
      't6',
      'Edit',
      { file_path: '/x/a.rs', old_string: 'one', new_string: 'two' },
      {
        type: 'tool_result',
        content: '<tool_use_error>File has not been read yet.</tool_use_error>',
        is_error: true,
      },
    );
    expect(quiet.body.at(-1), 'a one-line envelope carries no detail').toEqual({
      kind: 'error',
      message: 'File has not been read yet.',
      detail: '',
    });
  });

  it('draws a completed result verbatim even when it quotes both tags', () => {
    // **The failure direction is the only one the envelope belongs to.** Ten
    // real completed results on this machine carry both tags - Reads of forge
    // source, a diff, a grep for the string - and unwrapping those rewrites
    // what the tool actually said (a 23,305-character Read collapsing to a
    // bogus hint). The terminal gates the same question on Failed|Killed; a
    // completed run shows its output verbatim, tags and all.
    const quoting = leafOf(
      't7',
      'Read',
      { file_path: '/x/a.rs' },
      {
        type: 'tool_result',
        content: 'the error path writes <tool_use_error> and </tool_use_error> around it',
      },
    );

    expect(quoting.body.at(-1), 'a completed result keeps its own words').toEqual({
      kind: 'text',
      text: 'the error path writes <tool_use_error> and </tool_use_error> around it',
    });
  });

  it('reads the envelope off a failed result whose content is a block array', () => {
    // The MCP-result shape: an array of blocks rather than a string, which is
    // where the wrapped refusals of the forge tools arrive.
    const failed = leafOf(
      't8',
      'mcp__forge__agents__tell',
      { label: 'companies', message: 'picking it up' },
      {
        type: 'tool_result',
        content: [
          {
            type: 'text',
            text: '<tool_use_error>no label companies under this project</tool_use_error>',
          },
        ],
        is_error: true,
      },
    );

    expect(failed.body.at(-1), 'the array branch reads the envelope too').toEqual({
      kind: 'error',
      message: 'no label companies under this project',
      detail: '',
    });
  });
});

describe('what opens without being asked', () => {
  /** A mutation whose two sides carry `lines` lines each. */
  const edit = (lines: number) => {
    const side = Array.from({ length: lines }, (_, n) => `line ${n}`).join('\n');
    return leafOf(
      't1',
      'Edit',
      { file_path: '/x/a.rs', old_string: side, new_string: side },
      answered('The file /x/a.rs has been updated.'),
    );
  };

  it('opens a mutation while its diff is small enough to draw, and not past the bound', () => {
    // The bound is why this exists: an open diff of ~245,000px pinned the
    // renderer on the seat that held it (WebKit re-lays every mounted giant on
    // each pass), so a very large one must start closed rather than be drawn
    // unbounded. Dropping the size term re-opens every giant diff silently.
    const small = edit(10);
    expect(
      opensByDefault(small.name, small.body),
      'an ordinary mutation draws its diff without being asked',
    ).toBe(true);

    const huge = edit(1500);
    expect(
      opensByDefault(huge.name, huge.body),
      'a mutation over the bound starts closed, and one click still opens it',
    ).toBe(false);

    // The unit is rows, not newlines: a real diff carried a 441,094-character
    // line, which a newline count reads as one line and the drawing wraps into
    // ~245,000px of rows. Counting newlines let exactly that shape through.
    const oneLongLine = leafOf(
      't3',
      'Edit',
      { file_path: '/x/gen.rs', old_string: 'x'.repeat(441_094), new_string: 'y'.repeat(441_094) },
      answered('The file /x/gen.rs has been updated.'),
    );
    expect(
      opensByDefault(oneLongLine.name, oneLongLine.body),
      'a few enormous lines are rows in the hundreds of thousands, and stay closed',
    ).toBe(false);

    const bash = leafOf('t2', 'Bash', { command: 'ls' }, answered('a.rs\nb.rs'));
    expect(opensByDefault(bash.name, bash.body), 'a call with no diff never opens itself').toBe(
      false,
    );
  });
});

/**
 * How many times anything walked a string through its character iterator while
 * `fn` ran.
 *
 * The reader below reads each hunk line by its mark, and one way of writing it
 * walks the whole line: `const [mark, ...rest] = line` iterates the string into
 * one array entry and one string per character, which on the line this was
 * measured against - 439,612 characters - is some 440,000 allocations for that
 * one line. The fold re-reads every hunk of a turn on every frame the seat
 * emits, so the count is the assertion rather than an elapsed time. It sees the
 * iterator only: a `line.split('')` rewrite is the same allocation class and
 * would pass at 0.
 */
function stringSteps(fn: () => void): number {
  const native = String.prototype[Symbol.iterator];
  let steps = 0;
  String.prototype[Symbol.iterator] = function (this: string): StringIterator<string> {
    const held = native.call(this);
    const counting = {
      next: (): IteratorResult<string> => {
        steps += 1;
        return held.next();
      },
    };
    // The declared iterator type carries helper methods a spread never calls;
    // `next` is the whole of what the reader under test walks it with.
    return counting as unknown as StringIterator<string>;
  };
  try {
    fn();
  } finally {
    String.prototype[Symbol.iterator] = native;
  }
  return steps;
}

describe('what one hunk line costs to read', () => {
  it('reads a line by its mark, with no character-iterator walk', () => {
    // Not hypothetical: two Edit results in the inbox-triage transcript carry
    // lines this size in their `structuredPatch` (439,612 characters), and
    // reading them that way was 23% of the client's busy time while the seat
    // streams (seat mirror, 2026-10-03).
    const long = 'x'.repeat(200_000);
    let body: CallBody[] = [];
    const steps = stringSteps(() => {
      const leaf = leafOf(
        't1',
        'Edit',
        { file_path: '/x/a.rs' },
        answered('The file /x/a.rs has been updated.'),
        {
          structuredPatch: [
            {
              oldStart: 1,
              oldLines: 3,
              newStart: 1,
              newLines: 3,
              lines: [`+${long}`, `-${long}`, ` ${long}`],
            },
          ],
        },
      );
      body = leaf.body;
    });

    expect(steps, 'a line is read with no character-iterator walk').toBe(0);
    const [hunk] = body;
    if (hunk?.kind !== 'hunk') throw new Error('the row drew no hunk');
    expect(
      hunk.lines.map((line) => [line.kind, line.text.length]),
      'each line read by its mark, its text whole',
    ).toEqual([
      ['add', long.length],
      ['del', long.length],
      ['ctx', long.length + 1],
    ]);
    expect(hunk.lines[0]?.text, 'and a change loses its mark, which the row draws itself').toBe(
      long,
    );
  });
});
