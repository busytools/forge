import { describe, expect, it } from 'vitest';

import { familyOf, iconOf, rowOf, taskStatus } from './families';

describe('the row a call is summarised under', () => {
  it('keys a family tool on its family', () => {
    expect(rowOf('Read')).toEqual({ kind: 'family', family: 'read' });
    expect(familyOf('Grep')).toBe('search');
  });

  it('gives every mutation the one edit row', () => {
    // The wire's own answer gives each of the four tools a row of its own,
    // which draws two rows both reading `edit` with the second out of order
    // behind the first.
    for (const name of ['Edit', 'Write', 'MultiEdit', 'NotebookEdit']) {
      expect(rowOf(name), `${name} is a mutation`).toEqual({ kind: 'family', family: 'edit' });
    }
  });

  it('keys an unknown MCP call to the mcp row', () => {
    // A server's name is known only at runtime, and a row draws the mcp glyph
    // rather than the server: two servers are two rows of one kind.
    expect(rowOf('mcp__otherserver__thing')).toEqual({ kind: 'mcp' });
  });

  it('gives the browser tools their own row and globe', () => {
    // The sessions drive the client's own browser, so the row names what it
    // drives rather than the driver's server that carried it - and a browser
    // call is one whatever server the name wore.
    for (const name of [
      'mcp__playwright__browser_click',
      'mcp__playwright__browser_navigate',
      'mcp__forge__browser_snapshot',
      'browser_hand_off',
    ]) {
      expect(rowOf(name), `${name} is a browser call`).toEqual({ kind: 'browser' });
      expect(iconOf(rowOf(name)), `${name} draws the globe`).toBe('web');
    }
    // A tool merely NAMING the browser is not one.
    expect(rowOf('mcp__otherserver__browsing')).toEqual({ kind: 'mcp' });
  });

  it('gives the systemone decisions their own row and fork', () => {
    // The three decisions are the one MCP group with a class of its own: a
    // reader scanning for what the session decided must not find them mixed
    // among the cron and peer calls that share the forge server.
    for (const name of [
      'mcp__forge__systemone__ask_noul',
      'mcp__forge__systemone__ask_choice',
      'mcp__forge__systemone__ask_score',
    ]) {
      expect(rowOf(name), `${name} is a decision call`).toEqual({ kind: 'systemone' });
      expect(iconOf(rowOf(name)), `${name} draws the fork`).toBe('decide');
    }
  });

  it('keys a forge family tool to its own family and glyph', () => {
    // The family is the first segment under the server, read by name rather
    // than listed: a family the server grows arrives here by being called.
    for (const [name, family, glyph] of [
      ['mcp__forge__tasks__update', 'tasks', 'tasks'],
      ['mcp__forge__cron__create', 'cron', 'schedules'],
      ['mcp__forge__gotify__subscribe', 'gotify', 'gotify'],
      ['mcp__forge__slack__post', 'slack', 'slack'],
      ['mcp__forge__review__reply', 'review', 'review'],
      ['mcp__forge__agents__spawn', 'agents', 'subagents'],
    ] as const) {
      expect(rowOf(name), `${name} is a forge family call`).toEqual({ kind: 'forge', family });
      expect(iconOf(rowOf(name)), `${name} draws its family glyph`).toBe(glyph);
    }
    // The class is the three decision names, not the server prefix: a future
    // `systemone__*` tool that is not one of the decisions stays an mcp row.
    expect(rowOf('mcp__forge__systemone__something_else')).toEqual({ kind: 'mcp' });
  });

  it('draws a tool the table has no row for as the generic one', () => {
    expect(rowOf('brand_new_tool')).toEqual({ kind: 'family', family: 'tool' });
    expect(rowOf('Monitor')).toEqual({ kind: 'family', family: 'tool' });
  });

  it('names the sprite each row draws', () => {
    expect(iconOf(rowOf('Read'))).toBe('read');
    expect(iconOf(rowOf('Edit'))).toBe('edit');
    expect(iconOf(rowOf('mcp__otherserver__thing'))).toBe('mcp');
    expect(iconOf(rowOf('brand_new_tool'))).toBe('tool');
    // Three families draw a symbol of a nearer name than their own: the boxed
    // terminal, the settings gear and the git branch - the family words name
    // no sprite.
    expect(iconOf(rowOf('Bash'))).toBe('square-terminal');
    expect(iconOf(rowOf('Config'))).toBe('settings');
    expect(iconOf(rowOf('EnterWorktree'))).toBe('git');
  });
});

describe('the status word a task frame carries', () => {
  it('reads the words the wire sends as the statuses a row draws', () => {
    // The wire says `running` where the row says in progress, and `stopped` is
    // its word for a graceful cancel, which draws as the kill it is.
    expect(taskStatus('running')).toBe('in_progress');
    expect(taskStatus('completed')).toBe('completed');
    expect(taskStatus('failed')).toBe('failed');
    expect(taskStatus('killed')).toBe('killed');
    expect(taskStatus('stopped')).toBe('killed');
  });

  it('gives back nothing for a word it does not know, rather than a guess', () => {
    // `completed` is the guess that would look right and be wrong: the caller
    // keeps the status the call already had.
    expect(taskStatus('half-done')).toBeNull();
    expect(taskStatus(null)).toBeNull();
  });
});
