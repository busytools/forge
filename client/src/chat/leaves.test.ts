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
  it('draws a mutation as its diff, without the note the CLI writes to the model', () => {
    const leaf = leafOf(
      't1',
      'Edit',
      { file_path: 'a.rs', old_string: 'one', new_string: 'two' },
      answered(NOTE),
      null,
    );

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
    const bash = leafOf('t2', 'Bash', { command: 'ls' }, answered('a.rs\nb.rs'), null);
    expect(
      bash.body.map((part) => part.kind),
      'a call with no diff still says what it said',
    ).toEqual(['text']);
  });
});
