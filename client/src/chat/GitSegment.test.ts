import { render } from 'svelte/server';
import { afterEach, describe, expect, it, vi } from 'vitest';

import GitSegment from './GitSegment.svelte';
import { changeLine } from './change';
import { git, vcsLine } from './git.svelte';

afterEach(() => {
  git.show(null);
  git.sync(null);
  vi.useRealTimers();
});

describe('the tree segment', () => {
  it('carries the tree glyph and the branch line', () => {
    git.sync({
      label: 'web-home-layout \u{b7} 3 files',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    const body = render(GitSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-git');
    expect(body, 'the branch and the count read on the toggle').toContain(
      'web-home-layout \u{b7} 3 files',
    );
  });

  it('draws nothing where the record has not landed', () => {
    git.sync(null);
    const body = render(GitSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });

  it("puts a repository event on the shown seat's strip as one transient line", () => {
    const slot = { org: 'TestOrg', project: 'forge', label: 'lead' };
    git.show(slot);
    git.sync({
      label: 'web-home-layout \u{b7} 3 files',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    git.note(slot, vcsLine('commit', 'main'));
    const body = render(GitSegment, {}).body;

    expect(body, 'the event rides beside the branch it moved').toContain('committed on main');
    expect(git.noteNow()?.line).toBe('committed on main');
  });

  it('names a mutation the build has no verb for, and a branchless one', () => {
    expect(vcsLine('checkout', 'topic')).toBe('repository changed (checkout) on topic');
    expect(vcsLine('merge', null)).toBe('merged');
    expect(vcsLine('push', 'main')).toBe('pushed main');
  });

  it("gives the row back to the tree after the event's moment", () => {
    vi.useFakeTimers();
    const slot = { org: 'TestOrg', project: 'forge', label: 'lead' };
    git.show(slot);
    git.note(slot, 'committed on main');
    expect(git.noteNow()?.line, 'the event is on the row').toBe('committed on main');

    vi.advanceTimersByTime(8_000);
    expect(git.noteNow(), 'the event clears itself').toBeNull();
  });

  it('draws the note with no tree row at all, as on a clean default branch', () => {
    const slot = { org: 'TestOrg', project: 'forge', label: 'lead' };
    git.show(slot);
    git.sync(null);
    git.note(slot, 'committed on main');
    const body = render(GitSegment, {}).body;

    expect(body, 'the event reaches the strip even with no tree row').toContain(
      'committed on main',
    );
    expect(git.anything(), 'the strip row is present for it').toBe(true);
    expect(body, 'the note is a live region for assistive tech').toContain('role="status"');
  });

  it('drops a live note when the strip moves to another seat', () => {
    const lead = { org: 'TestOrg', project: 'forge', label: 'lead' };
    const worker = { ...lead, label: 'w1' };
    git.show(lead);
    git.note(lead, 'committed on main');
    expect(git.noteNow()?.line).toBe('committed on main');

    git.show(worker);
    expect(git.noteNow(), "the next seat does not inherit the last one's event").toBeNull();
  });

  it("names a published action in the strip's own words, including the flag names", () => {
    expect(changeLine('github', '123', 'created', 'example/example')).toBe(
      'PR #123 created in example/example',
    );
    expect(changeLine('gitlab', '45', 'merged', 'group/proj')).toBe('MR #45 merged in group/proj');
    expect(changeLine('bitbucket', '11', 'closed', 'team/app')).toBe('PR #11 closed in team/app');
    expect(changeLine('github-enterprise', '7', 'created', 'octo/repo')).toBe(
      'PR #7 created in octo/repo',
    );
    expect(changeLine('github', '123', 'edited', 'example/example')).toBe(
      'PR #123 edited in example/example',
    );
    expect(changeLine('github', '123', 'checked-out', 'example/example')).toBe(
      'PR #123 checked out in example/example',
    );
    expect(changeLine('github', '123', 'auto-merge-enabled', 'example/example')).toBe(
      'PR #123 auto-merge on in example/example',
    );
    expect(changeLine('github', '123', 'auto-merge-disabled', 'example/example')).toBe(
      'PR #123 auto-merge off in example/example',
    );
    expect(changeLine(null, null, null, null)).toBe('change changed');
    expect(changeLine('github', '', '', ''), 'empty fields leave no stray marks').toBe(
      'PR changed',
    );
  });
});
