import { describe, expect, it } from 'vitest';

import { aggregateStatus, familyOf, iconOf, labelOf, rowOf, taskStatus } from './families';

describe('the row a call is summarised under', () => {
  it('keys a family tool on its family, and the row draws the family word', () => {
    expect(rowOf('Read')).toEqual({ kind: 'family', family: 'read' });
    expect(labelOf('Read')).toBe('read');
    expect(familyOf('Grep')).toBe('search');
    expect(labelOf('Grep')).toBe('search');
  });

  it('gives every mutation the one edit row', () => {
    // The wire's own answer gives each of the four tools a row of its own,
    // which draws two rows both labelled `edit` with the second out of order
    // behind the first.
    for (const name of ['Edit', 'Write', 'MultiEdit', 'NotebookEdit']) {
      expect(rowOf(name), `${name} is a mutation`).toEqual({ kind: 'family', family: 'edit' });
      expect(labelOf(name), `${name} draws the fold's own word`).toBe('edit');
    }
  });

  it('gives each MCP server a lane named for the server', () => {
    // Two servers are two rows, and a server's name is only known at runtime,
    // which is why the row carries a label rather than a family alone.
    expect(rowOf('mcp__playwright__browser_click')).toEqual({ kind: 'mcp' });
    expect(labelOf('mcp__playwright__browser_click')).toBe('playwright');
    expect(labelOf('mcp__forge__agents__list')).toBe('forge');
  });

  it('draws a tool the table has no row for as the generic one', () => {
    expect(rowOf('brand_new_tool')).toEqual({ kind: 'family', family: 'tool' });
    expect(labelOf('brand_new_tool')).toBe('tool');
    expect(labelOf('Monitor')).toBe('tool');
  });

  it('names the sprite each row draws', () => {
    expect(iconOf(rowOf('Read'))).toBe('read');
    expect(iconOf(rowOf('Edit'))).toBe('edit');
    expect(iconOf(rowOf('mcp__forge__agents__list'))).toBe('mcp');
    expect(iconOf(rowOf('brand_new_tool'))).toBe('tool');
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

describe('what a run reports', () => {
  it('reports a failure in the run, and the calls keep their own status', () => {
    expect(aggregateStatus(['completed', 'failed', 'completed'])).toBe('failed');
  });

  it('reports a run still going as in progress', () => {
    expect(aggregateStatus(['completed', 'in_progress'])).toBe('in_progress');
  });

  it('reports a run with nothing back yet as pending', () => {
    expect(aggregateStatus(['completed', 'pending'])).toBe('pending');
    expect(aggregateStatus(['completed', 'completed'])).toBe('completed');
  });
});
