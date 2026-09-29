import { describe, expect, it } from 'vitest';

import type { Turn } from './conversation';
import { rowsOf } from './rows';

/** A turn holding `messages`, which is what a page hands the chat. */
const turn = (...messages: unknown[]): Turn => ({ key: 't1', messages, live: false });

/** An assistant frame carrying `content`. */
const said = (content: unknown[], id = 'm1'): unknown => ({
  type: 'assistant',
  message: { id, role: 'assistant', model: 'claude-opus-5', content },
});

/** A user frame, which is where both prompts and tool results arrive. */
const heard = (content: unknown[], uuid = 'u1'): unknown => ({
  type: 'user',
  message: { role: 'user', content },
  uuid,
});

const text = (value: string): unknown => ({ type: 'text', text: value });
const use = (id: string, name: string, input: unknown): unknown => ({
  type: 'tool_use',
  id,
  name,
  input,
});
const result = (id: string, value: string, isError = false): unknown => ({
  type: 'tool_result',
  tool_use_id: id,
  content: value,
  is_error: isError,
});

describe('one turn, as the rows it draws', () => {
  it('draws the prose and the calls in the order the turn wrote them', () => {
    const rows = rowsOf(
      turn(
        said([
          text('reading the family table'),
          use('c1', 'Read', {
            file_path: '/Users/ved/Projects/forge/crates/forge-server/src/x.rs',
          }),
        ]),
        heard([result('c1', 'the file')]),
        said([text('now the grouping')], 'm2'),
      ),
      '/Users/ved/Projects/forge',
    );

    expect(rows.map((row) => row.kind)).toEqual(['prose', 'call', 'prose']);
    expect(rows[0]).toMatchObject({ kind: 'prose', mine: false, text: 'reading the family table' });
    expect(rows[1]).toMatchObject({
      kind: 'call',
      name: 'Read',
      // Shortened against the tree the session is in: the prefix is the same
      // on every row of a conversation, so it says nothing and costs width.
      title: 'crates/forge-server/src/x.rs',
      status: 'completed',
    });
    expect(rows[2]).toMatchObject({ kind: 'prose', text: 'now the grouping' });
  });

  it('does not draw a tool result as something the reader said', () => {
    // A result arrives in a USER frame, because that is the shape the wire
    // uses for it. Drawn as a turn, the page puts the tool's own output in a
    // bubble attributed to the person, which is the defect this pins.
    const rows = rowsOf(
      turn(
        heard([text('run the gate')]),
        said([use('c1', 'Bash', { command: 'just check' })]),
        heard([result('c1', 'all green')]),
      ),
      null,
    );

    const mine = rows.filter((row) => row.kind === 'prose' && row.mine);
    expect(mine, 'the only thing the reader said is what they said').toHaveLength(1);
    expect(mine[0]).toMatchObject({ text: 'run the gate' });
    expect(rows.filter((row) => row.kind === 'call')).toHaveLength(1);
  });

  it('strips the escape sequences out of what a command produced', () => {
    // The server no longer takes these off, so a row that draws them raw
    // shows the sequence's own parameters - and an unterminated one eats the
    // rest of the line rather than showing it.
    const rows = rowsOf(
      turn(
        said([use('c1', 'Bash', { command: 'just check' })]),
        heard([result('c1', '\u{1b}[32mSummary\u{1b}[0m [92s]\n\u{1b}[38;5;12')]),
      ),
      null,
    );

    const [call] = rows;
    expect(call).toMatchObject({ kind: 'call' });
    expect(call?.kind === 'call' ? call.body : []).toEqual([
      { kind: 'text', text: 'Summary [92s]\n' },
    ]);
  });

  it('marks the call whose result failed, and only that one', () => {
    const rows = rowsOf(
      turn(
        said([use('c1', 'Read', { file_path: 'a.rs' }), use('c2', 'Read', { file_path: 'b.rs' })]),
        heard([result('c1', 'no such file', true), result('c2', 'the file')]),
      ),
      null,
    );

    expect(rows.map((row) => (row.kind === 'call' ? row.status : null))).toEqual([
      'failed',
      'completed',
    ]);
  });

  it('leaves a call that has not come back as pending', () => {
    const rows = rowsOf(turn(said([use('c1', 'Bash', { command: 'just check' })])), null);

    expect(rows[0]).toMatchObject({ kind: 'call', status: 'pending', command: 'just check' });
  });

  it('keeps the command a call ran beside the description that named it', () => {
    // A described call is named by its description, so the command it actually
    // ran would otherwise appear nowhere in the turn.
    const rows = rowsOf(
      turn(said([use('c1', 'Bash', { command: 'just check', description: 'run the gate' })])),
      null,
    );

    expect(rows[0]).toMatchObject({ kind: 'call', title: 'run the gate', command: 'just check' });
  });

  it('draws a mutation as the diff its own input carries', () => {
    const rows = rowsOf(
      turn(
        said([
          use('c1', 'Edit', {
            file_path: 'crates/forge-web/src/home.css',
            old_string: 'flex: none;',
            new_string: 'flex: 0 1 auto;',
          }),
        ]),
      ),
      null,
    );

    expect(rows[0]).toMatchObject({
      kind: 'call',
      title: 'crates/forge-web/src/home.css',
      body: [
        {
          kind: 'diff',
          path: 'crates/forge-web/src/home.css',
          old: 'flex: none;',
          new: 'flex: 0 1 auto;',
        },
      ],
    });
  });

  it('draws a Write as the file it wrote, on the added side', () => {
    // A Write carries the whole file rather than a change to it. Read the
    // other way round, a file the session just wrote draws as a file it
    // deleted: every line on the red ground with a removed mark, in a card
    // that opens without being clicked.
    const rows = rowsOf(
      turn(
        said([
          use('c1', 'Write', {
            file_path: 'crates/forge-web/src/home.css',
            content: 'line one\nline two',
          }),
        ]),
      ),
      null,
    );

    expect(rows[0]).toMatchObject({
      kind: 'call',
      body: [{ kind: 'diff', old: '', new: 'line one\nline two' }],
    });
  });

  it('draws each edit of a MultiEdit as its own hunk', () => {
    const rows = rowsOf(
      turn(
        said([
          use('c1', 'MultiEdit', {
            file_path: 'a.rs',
            edits: [
              { old_string: 'one', new_string: 'two' },
              { old_string: 'three', new_string: 'four' },
            ],
          }),
        ]),
      ),
      null,
    );

    expect(rows[0]).toMatchObject({
      kind: 'call',
      body: [
        { kind: 'diff', old: 'one', new: 'two' },
        { kind: 'diff', old: 'three', new: 'four' },
      ],
    });
  });

  it('draws no diff at all for a mutation whose input says nothing', () => {
    // An unread shape must not open an empty card: a mutation's body shows
    // without being clicked, so a blank one is what the reader sees.
    const rows = rowsOf(
      turn(
        said([
          use('c1', 'Write', { file_path: 'a.rs', content: '   \n  ' }),
          use('c2', 'MultiEdit', { file_path: 'a.rs' }),
        ]),
      ),
      null,
    );

    expect(rows.map((row) => (row.kind === 'call' ? row.body : null))).toEqual([[], []]);
  });

  it('names each row the same way every time the turn is read', () => {
    // The rows are keyed so that growing one turn redraws only what changed.
    // A key that moved between two reads of the same turn is a row that is
    // thrown away and rebuilt on every frame.
    const held = turn(
      said([text('one'), use('c1', 'Read', { file_path: 'a.rs' })]),
      heard([result('c1', 'the file')]),
    );

    expect(rowsOf(held, null).map((row) => row.key)).toEqual(
      rowsOf(held, null).map((row) => row.key),
    );
    expect(new Set(rowsOf(held, null).map((row) => row.key)).size, 'and each is its own').toBe(2);
  });

  it('draws a call whose input this page cannot name by its own name', () => {
    const rows = rowsOf(turn(said([use('c1', 'mcp__forge__agents__list', {})])), null);

    expect(rows[0]).toMatchObject({ kind: 'call', title: 'agents__list' });
  });
});
